// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GarbageCollector trait -- abstraction over heap implementations.
//!
//! Allows swapping between the simple semi-space `Heap` and the
//! generational `GenerationalHeap` without changing call sites.

use std::collections::HashMap;

use crate::gc::GcResult;
use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind};
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Volatile field atomicity — striped locks
// ---------------------------------------------------------------------------
//
// JLS §17.7 requires reads and writes of `volatile long` / `volatile double`
// (and by extension all `volatile`-declared fields) to be atomic. The CratonVM
// heap stores fields as 16-byte `Value` (8-byte tag + 8-byte payload), which
// is wider than any stable Rust atomic primitive on x86-64 — a SeqCst fence
// pair gives the required happens-before ordering but does **not** guarantee
// atomicity of the 16-byte slot write itself. A concurrent volatile read
// could otherwise observe a torn (tag, payload) pair from two overlapping
// writes.
//
// We provide atomicity with a small striped-mutex pool (`parking_lot::Mutex`
// is uncontended-fast and avoids OS calls). The previous design used a single
// heap-wide mutex which serialized every volatile op across the whole heap
// — striping by `(obj_ref, index)` removes that bottleneck while preserving
// per-slot atomicity. `Mutex<()>` is the minimum primitive that gives mutual
// exclusion without storing the data inside the lock.
//
// Stripe count is a power of two so the index modulo is a single AND. 64 is
// a balance between false-sharing (a too-small pool serializes unrelated
// fields) and memory (64 × ~5 bytes is negligible).
const VOLATILE_STRIPE_COUNT: usize = 64;

static VOLATILE_STRIPES: std::sync::OnceLock<[parking_lot::Mutex<()>; VOLATILE_STRIPE_COUNT]> =
    std::sync::OnceLock::new();

#[inline]
fn volatile_stripes() -> &'static [parking_lot::Mutex<()>; VOLATILE_STRIPE_COUNT] {
    VOLATILE_STRIPES.get_or_init(|| std::array::from_fn(|_| parking_lot::Mutex::new(())))
}

/// Acquire the stripe lock guarding volatile reads/writes for the given
/// `(obj_ref, index)`. The hash spreads keys across [`VOLATILE_STRIPE_COUNT`]
/// stripes so unrelated volatile fields rarely contend. The caller must
/// hold the returned guard for the duration of the slot read or write.
#[inline]
pub fn volatile_stripe_lock(
    obj_ref: ObjectRef,
    index: usize,
) -> parking_lot::MutexGuard<'static, ()> {
    // Mix the object address (already 8-byte-aligned, so low 3 bits are 0)
    // with the slot index. A multiplicative mix gives even distribution
    // across stripes for both small object pools and large sequentially-
    // allocated heaps.
    let addr = obj_ref.as_ptr() as usize;
    let mixed = (addr.wrapping_shr(3))
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(index);
    let stripe = mixed & (VOLATILE_STRIPE_COUNT - 1);
    volatile_stripes()[stripe].lock()
}

/// Trait for monitor table cleanup after GC relocation.
///
/// The VM implements this for its `MonitorTable` so the gc crate does not
/// need to depend on VM-internal types.
pub trait MonitorCleanup {
    /// Re-key monitors using the old-address-to-new-address mapping.
    fn remap_after_gc(&self, pointer_map: &HashMap<usize, usize>);
}

/// Trait abstracting a garbage-collected heap.
///
/// Both the simple semi-space `Heap` and the generational
/// `GenerationalHeap` implement this trait. The VM accesses the heap
/// exclusively through these methods.
pub trait GarbageCollector: Send + Sync {
    // -- Allocation --

    /// Allocate a new Java object with `num_fields` field slots, all zeroed.
    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef;

    /// Allocate a new Java array with the given element type and length.
    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef;

    // -- Header access --

    /// Read the object header from a heap reference.
    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader;

    /// Get the class id of a heap object.
    fn class_id_of(&self, obj: ObjectRef) -> ClassId;

    /// Get the kind (Object or Array) of a heap allocation.
    fn kind_of(&self, obj: ObjectRef) -> ObjectKind;

    /// Get the element type of an array object.
    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType;

    /// Get the identity hash code of a heap object.
    fn identity_hash_code(&self, obj: ObjectRef) -> i32;

    // -- Field access --

    /// Get the value of a field at the given index.
    fn get_field(&self, obj: ObjectRef, index: usize) -> Value;

    /// Set the value of a field at the given index.
    fn set_field(&self, obj: ObjectRef, index: usize, value: Value);

    /// Get the value of a volatile field at the given index.
    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value;

    /// Set the value of a volatile field at the given index.
    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value);

    /// T10.9.E — Descriptor-aware field read.
    ///
    /// Reads the slot via [`get_field`](Self::get_field) and then normalizes
    /// the returned `Value` to match the declared field type encoded as a
    /// JVM descriptor byte (`b'J'`, `b'D'`, `b'L'`, etc.). This guarantees
    /// that a long-typed field never surfaces as `Value::Double` even if an
    /// upstream putfield corrupted the slot tag via a CompactValue round
    /// trip.
    ///
    /// Default implementation: raw read + coercion. Collectors may override
    /// for a fused fast path; the default is always correct.
    fn get_field_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field(obj, index);
        crate::heap::coerce_field_value_by_descriptor(raw, desc_byte)
    }

    /// Volatile descriptor-aware read.
    fn get_field_volatile_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field_volatile(obj, index);
        crate::heap::coerce_field_value_by_descriptor(raw, desc_byte)
    }

    /// Descriptor-aware write — normalizes the stored `Value` to match the
    /// declared field type before the underlying slot write.
    fn set_field_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        let coerced = crate::heap::coerce_field_value_by_descriptor(value, desc_byte);
        self.set_field(obj, index, coerced);
    }

    /// Volatile descriptor-aware write.
    fn set_field_volatile_as(
        &self,
        obj: ObjectRef,
        index: usize,
        value: Value,
        desc_byte: u8,
    ) {
        let coerced = crate::heap::coerce_field_value_by_descriptor(value, desc_byte);
        self.set_field_volatile(obj, index, coerced);
    }

    // -- Array access --

    /// Get the length of an array.
    fn array_length(&self, obj: ObjectRef) -> usize;

    /// Get an array element at the given index.
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32>;

    /// Set an array element at the given index.
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32>;

    // -- GC operations --

    /// Returns true when the heap should be garbage collected.
    fn needs_gc(&self) -> bool;

    /// Run a garbage collection cycle.
    fn collect_garbage(&self, roots: &mut [ObjectRef], monitors: &dyn MonitorCleanup) -> GcResult;

    /// Write barrier -- called **after** every reference store into a heap
    /// object.
    ///
    /// For the simple heap this is a no-op. The generational heap marks
    /// the card table entry dirty when an old-gen object stores a reference
    /// to a young-gen object. G1 records the cross-region edge in the
    /// destination region's remembered set.
    ///
    /// # SATB pre-barrier precondition
    ///
    /// This trait method is a **post-store** hook: the new reference value
    /// is already in the slot by the time it fires. Concurrent collectors
    /// that use Snapshot-At-The-Beginning (SATB) marking (currently G1) also
    /// require the *old* slot value to be logged **before** the store —
    /// otherwise objects reachable only through the overwritten reference
    /// at the start of the marking cycle can be lost (lost-object bug,
    /// downstream UAF on the next collection).
    ///
    /// When the underlying collector advertises `is_marking_active()` (see
    /// `VmHeap::g1_is_marking_active`), callers MUST invoke
    /// `VmHeap::satb_barrier(old_value)` **before** the store whose
    /// completion is being signalled here. The trait shape cannot deliver
    /// the old value after the fact, so this two-stage protocol is a caller
    /// contract — there is no way for the GC alone to recover a missed
    /// pre-barrier. G1 ships a best-effort `debug_assert!` that fires when
    /// `write_barrier` is invoked while marking is active without any
    /// matching pre-call on the same thread; release builds skip the check.
    fn write_barrier(&self, obj: ObjectRef, stored_value: Value);

    /// Total bytes currently allocated.
    fn allocated_bytes(&self) -> usize;
}
