//! GarbageCollector trait -- abstraction over heap implementations.
//!
//! Allows swapping between the simple semi-space `Heap` and the
//! generational `GenerationalHeap` without changing call sites.

use std::collections::HashMap;

use crate::gc::GcResult;
use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind};
use rustjvm_types::{ClassId, ObjectRef, Value};

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

    /// Write barrier -- called after every reference store into a heap object.
    ///
    /// For the simple heap this is a no-op. The generational heap marks
    /// the card table entry dirty when an old-gen object stores a reference
    /// to a young-gen object.
    fn write_barrier(&self, obj: ObjectRef, stored_value: Value);

    /// Total bytes currently allocated.
    fn allocated_bytes(&self) -> usize;
}
