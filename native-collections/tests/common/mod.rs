// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! Minimal `NativeContext` mock for the GC-relocation overlay tests.
//!
//! Adapted from `native-io/src/test_support.rs` — same heap-entry
//! design, but with a key extra: `identity_hash_code` reads from a
//! per-ObjectRef hash map so the test can simulate a moving GC by
//! creating two distinct `ObjectRef` values that share the same
//! identity-hash word (the exact contract a real moving GC preserves
//! during compaction).
//!
//! The mock implements every `NativeContext` method that the trait
//! requires. Methods the overlay shims don't call return cheap
//! defaults so the file stays under 300 LOC without losing
//! type-soundness.

#![allow(dead_code, unused_variables)]

use std::cell::UnsafeCell;
use std::collections::HashMap;

use cratonvm_native_api::{
    AnnotationData, AnnotationElementValue, FieldMetadata, MethodMetadata, NativeContext,
    StackTraceEntry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

enum HeapEntry {
    Object { fields: Vec<Value> },
    Array { elements: Vec<Value> },
}

/// Deterministic pointer-to-i32 hash with a per-MockCtx namespace
/// offset. Same `(ptr, namespace)` always yields the same `i32` so
/// the contract `identity_hash_code(x) == identity_hash_code(x)` holds.
/// Crucially, the namespace is mixed in via XOR after the multiplier
/// step so two MockCtx instances allocate hashes from disjoint
/// 32-bit subranges — preventing process-global overlay collisions
/// when Cargo runs integration tests in parallel.
fn derive_hash(ptr: usize, namespace: i32) -> i32 {
    let mixed = ptr.wrapping_mul(0x9E37_79B9_usize) ^ 0xCAFE_BABE_usize;
    let h = (mixed as i32) ^ namespace;
    if h == 0 { 0x1000_0001 } else { h }
}

pub struct MockCtx {
    heap: UnsafeCell<Vec<HeapEntry>>,
    // raw `as_ptr() as usize` -> heap-entry index
    ptr_to_index: UnsafeCell<HashMap<usize, usize>>,
    // raw `as_ptr() as usize` -> stable identity-hash word
    //
    // A real moving GC copies the identity-hash word along with the
    // object header during compaction. The mock mirrors that by
    // keeping the i32 hash in a side map: when `relocate_object`
    // creates a fresh `ObjectRef` for a relocated object, it inserts
    // the same hash under the new pointer, so
    // `identity_hash_code(old) == identity_hash_code(new)`.
    ptr_to_hash: UnsafeCell<HashMap<usize, i32>>,
    // Per-MockCtx hash-namespace offset. Cargo runs integration tests
    // in parallel by default; without a per-instance offset every
    // test's first object hashes to the same value, polluting the
    // process-global LHM/LL/TM/TS overlay statics across tests.
    // `CTX_COUNTER` advances by a 64-bit prime so two MockCtx
    // instances can never collide on derived hashes.
    hash_namespace: i32,
    next_ptr: usize,
}

impl Default for MockCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl MockCtx {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicI32, Ordering};
        static CTX_COUNTER: AtomicI32 = AtomicI32::new(1);
        let id = CTX_COUNTER.fetch_add(1, Ordering::Relaxed);
        // Multiplier is a large 31-bit prime — gives every MockCtx a
        // disjoint hash range from the others.
        let hash_namespace = id.wrapping_mul(0x7FFF_FFFF_u32 as i32);
        Self {
            heap: UnsafeCell::new(Vec::new()),
            ptr_to_index: UnsafeCell::new(HashMap::new()),
            ptr_to_hash: UnsafeCell::new(HashMap::new()),
            hash_namespace,
            next_ptr: 8,
        }
    }

    fn heap_mut(&self) -> &mut Vec<HeapEntry> {
        unsafe { &mut *self.heap.get() }
    }
    fn heap_ref(&self) -> &Vec<HeapEntry> {
        unsafe { &*self.heap.get() }
    }
    fn ptr_map_mut(&self) -> &mut HashMap<usize, usize> {
        unsafe { &mut *self.ptr_to_index.get() }
    }
    fn ptr_map_ref(&self) -> &HashMap<usize, usize> {
        unsafe { &*self.ptr_to_index.get() }
    }
    fn hash_map_mut(&self) -> &mut HashMap<usize, i32> {
        unsafe { &mut *self.ptr_to_hash.get() }
    }
    fn hash_map_ref(&self) -> &HashMap<usize, i32> {
        unsafe { &*self.ptr_to_hash.get() }
    }

    fn alloc_entry(&mut self, entry: HeapEntry) -> ObjectRef {
        let idx = self.heap_mut().len();
        self.heap_mut().push(entry);
        let ptr = self.next_ptr;
        self.next_ptr += 8;
        self.ptr_map_mut().insert(ptr, idx);
        unsafe { ObjectRef::from_raw(ptr as *mut u8) }
    }

    fn entry_index(&self, obj: ObjectRef) -> Option<usize> {
        self.ptr_map_ref().get(&(obj.as_ptr() as usize)).copied()
    }

    /// Allocate a regular object (no class binding) with `num_fields` slots.
    pub fn alloc_object_simple(&mut self, num_fields: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Object {
            fields: vec![Value::Object(None); num_fields],
        })
    }

    /// Simulate a moving-GC compaction: hands back a fresh `ObjectRef`
    /// pointing to a brand-new pointer address, while preserving the
    /// original object's identity-hash word (which a real GC copies
    /// from the source header into the destination header during
    /// `forward_object`). The new ObjectRef *also* aliases the same
    /// heap-entry index so loads/stores through it still hit the
    /// original fields — modelling the post-move "logical identity"
    /// the JVM presents to bytecode.
    pub fn relocate_object(&mut self, src: ObjectRef) -> ObjectRef {
        let src_ptr = src.as_ptr() as usize;
        let entry_idx = self
            .ptr_map_ref()
            .get(&src_ptr)
            .copied()
            .expect("relocate_object: source ObjectRef not in heap");
        // Lazily seed the source hash if it was never observed, so
        // `relocate_object` is callable on an object that the test
        // never read the identity hash of (e.g. a fresh alloc).
        if !self.hash_map_ref().contains_key(&src_ptr) {
            // Reuse the same derivation as `identity_hash_code` so
            // the seeded value matches what `identity_hash_code(src)`
            // would have returned. Without this, calling
            // `identity_hash_code(src)` *after* `relocate_object`
            // would compute a different hash than what we just
            // stored, breaking the post-move equality check.
            let derived = derive_hash(src_ptr, self.hash_namespace);
            self.hash_map_mut().insert(src_ptr, derived);
        }
        let src_hash = *self.hash_map_ref().get(&src_ptr).unwrap();
        // Allocate a new pointer address. Do NOT push a new heap
        // entry — point at the existing one so reads/writes through
        // the new ObjectRef still observe the relocated object's
        // state. (A real GC moves the bytes; we keep them in place
        // and just re-route the address.)
        let new_ptr = self.next_ptr;
        self.next_ptr += 8;
        self.ptr_map_mut().insert(new_ptr, entry_idx);
        self.hash_map_mut().insert(new_ptr, src_hash);
        unsafe { ObjectRef::from_raw(new_ptr as *mut u8) }
    }
}

impl NativeContext for MockCtx {
    fn new_array(&mut self, _et: ArrayElementType, length: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Array {
            elements: vec![Value::Int(0); length],
        })
    }
    fn array_length(&self, obj: ObjectRef) -> usize {
        match self.entry_index(obj) {
            Some(i) => match &self.heap_ref()[i] {
                HeapEntry::Array { elements } => elements.len(),
                _ => 0,
            },
            None => 0,
        }
    }
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
        match self.entry_index(obj) {
            Some(i) => match &self.heap_ref()[i] {
                HeapEntry::Array { elements } => {
                    elements.get(index).copied().unwrap_or(Value::Int(0))
                }
                _ => Value::Int(0),
            },
            None => Value::Int(0),
        }
    }
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
        if let Some(i) = self.entry_index(obj) {
            if let HeapEntry::Array { elements } = &mut self.heap_mut()[i] {
                if index < elements.len() {
                    elements[index] = value;
                }
            }
        }
    }
    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        match self.entry_index(obj) {
            Some(i) => match &self.heap_ref()[i] {
                HeapEntry::Object { fields } => {
                    fields.get(index).copied().unwrap_or(Value::Object(None))
                }
                _ => Value::Object(None),
            },
            None => Value::Object(None),
        }
    }
    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        if let Some(i) = self.entry_index(obj) {
            if let HeapEntry::Object { fields } = &mut self.heap_mut()[i] {
                if index >= fields.len() {
                    fields.resize(index + 1, Value::Object(None));
                }
                fields[index] = value;
            }
        }
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        let ptr = obj.as_ptr() as usize;
        if let Some(h) = self.hash_map_ref().get(&ptr).copied() {
            return h;
        }
        // First call for this pointer — derive a fresh hash from the
        // pointer plus this MockCtx's namespace, and stash it.
        // Subsequent calls return the same value (the "seed-on-
        // first-call" contract `identity_hash_code` follows in the
        // real GC heap).
        let h = derive_hash(ptr, self.hash_namespace);
        self.hash_map_mut().insert(ptr, h);
        h
    }

    // -- everything else: stubs (unused by the relocation harness) --

    fn load_class(&mut self, _n: &str) -> MethodCallResult {
        Ok(None)
    }
    fn new_object(&mut self, _c: &str) -> MethodCallResult {
        Ok(None)
    }
    fn invoke(&mut self, _c: &str, _m: &str, _d: &str, _a: &[Value]) -> MethodCallResult {
        Ok(None)
    }
    fn record_printed_value(&mut self, _v: Value) {}
    fn class_name_of_id(&self, _c: ClassId) -> Option<String> {
        None
    }
    fn class_id_of_object(&self, _o: ObjectRef) -> ClassId {
        ClassId::new(0)
    }
    fn capture_stack_trace(&mut self, _h: i32) -> Vec<StackTraceEntry> {
        Vec::new()
    }
    fn get_stack_trace(&self, _h: i32) -> Option<&[StackTraceEntry]> {
        None
    }
    fn get_field_by_name(&self, _o: ObjectRef, _n: &str) -> Value {
        Value::Object(None)
    }
    fn set_field_by_name(&self, _o: ObjectRef, _n: &str, _v: Value) {}
    fn resolve_field_index(&self, _c: &str, _f: &str) -> Option<usize> {
        None
    }
    fn method_exists(&self, _c: &str, _m: &str, _d: &str) -> bool {
        false
    }
    fn new_ref_array(&mut self, _c: ClassId, length: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Array {
            elements: vec![Value::Object(None); length],
        })
    }
    fn heap_kind_of(&self, _o: ObjectRef) -> ObjectKind {
        ObjectKind::Object
    }
    fn heap_element_type_of(&self, _o: ObjectRef) -> ArrayElementType {
        ArrayElementType::Reference
    }
    fn create_string(&mut self, _t: &str) -> ObjectRef {
        self.alloc_object_simple(0)
    }
    fn read_string(&self, _o: ObjectRef) -> Option<String> {
        None
    }
    fn get_class_mirror(&mut self, _c: ClassId) -> ObjectRef {
        self.alloc_object_simple(0)
    }
    fn record_printed_line(&mut self, _t: String) {}
    fn get_system_stream(&self, _n: &str) -> Option<ObjectRef> {
        None
    }
    fn get_system_property(&self, _k: &str) -> Option<String> {
        None
    }
    fn set_system_property(&mut self, _k: &str, _v: &str) -> Option<String> {
        None
    }
    fn alloc_object(&mut self, _c: ClassId, num_fields: usize) -> ObjectRef {
        self.alloc_object_simple(num_fields)
    }
    fn ensure_class_initialized(&mut self, _n: &str) -> Result<ClassId, MethodCallFailed> {
        Ok(ClassId::new(0))
    }
    fn is_subclass(&self, _c: ClassId, _p: ClassId) -> bool {
        false
    }
    fn superclass_of(&self, _c: ClassId) -> Option<ClassId> {
        None
    }
    fn is_interface_class(&self, _c: ClassId) -> bool {
        false
    }
    fn class_id_by_name(&self, _n: &str) -> Option<ClassId> {
        None
    }
    fn loader_id_of_class(&self, _c: ClassId) -> i32 {
        2
    }
    fn is_record_class(&self, _c: ClassId) -> bool {
        false
    }
    fn record_components(&self, _c: ClassId) -> Vec<(String, String)> {
        Vec::new()
    }
    fn is_sealed_class(&self, _c: ClassId) -> bool {
        false
    }
    fn permitted_subclasses(&self, _c: ClassId) -> Vec<String> {
        Vec::new()
    }
    fn object_num_fields(&self, obj: ObjectRef) -> usize {
        match self.entry_index(obj) {
            Some(i) => match &self.heap_ref()[i] {
                HeapEntry::Object { fields } => fields.len(),
                _ => 0,
            },
            None => 0,
        }
    }
    fn thread_id(&self) -> u64 {
        1
    }
    fn monitor_enter(&mut self, _o: ObjectRef) {}
    fn monitor_exit(&mut self, _o: ObjectRef) {}
    fn monitor_wait(&mut self, _o: ObjectRef, _t: Option<u64>) -> MethodCallResult {
        Ok(None)
    }
    fn monitor_notify(&mut self, _o: ObjectRef) -> MethodCallResult {
        Ok(None)
    }
    fn monitor_notify_all(&mut self, _o: ObjectRef) -> MethodCallResult {
        Ok(None)
    }
    fn thread_start(&mut self, _o: ObjectRef) -> MethodCallResult {
        Ok(None)
    }
    fn thread_join(&mut self, _o: ObjectRef) -> MethodCallResult {
        Ok(None)
    }
    fn thread_is_alive(&self, _o: ObjectRef) -> bool {
        false
    }
    fn current_thread_object(&mut self) -> ObjectRef {
        self.alloc_object_simple(0)
    }
    fn thread_interrupt(&mut self, _o: ObjectRef) {}
    fn is_interrupted(&self, _c: bool) -> bool {
        false
    }
    fn active_thread_count(&self) -> i32 {
        1
    }
    fn enumerate_threads(&self, _m: usize) -> Vec<ObjectRef> {
        Vec::new()
    }
    fn heap_allocated_bytes(&self) -> usize {
        0
    }
    fn loaded_class_count(&self) -> usize {
        0
    }
    fn gc_collection_count(&self) -> u64 {
        0
    }
    fn force_gc(&mut self) {}
    fn declared_fields(&self, _c: ClassId) -> Vec<FieldMetadata> {
        Vec::new()
    }
    fn declared_methods(&self, _c: ClassId) -> Vec<MethodMetadata> {
        Vec::new()
    }
    fn class_interfaces(&self, _c: ClassId) -> Vec<ClassId> {
        Vec::new()
    }
    fn class_access_flags(&self, _c: ClassId) -> u16 {
        0
    }
    fn get_static_field(&self, _c: ClassId, _i: usize) -> Value {
        Value::Int(0)
    }
    fn set_static_field(&mut self, _c: ClassId, _i: usize, _v: Value) {}
    fn primitive_class_mirror(&mut self, _n: &str) -> ObjectRef {
        self.alloc_object_simple(0)
    }
    fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
        use std::sync::OnceLock;
        static FD: OnceLock<cratonvm_native_api::fd_table::FileDescriptorTable> = OnceLock::new();
        FD.get_or_init(cratonvm_native_api::fd_table::FileDescriptorTable::new)
    }
    fn get_field_volatile(&self, o: ObjectRef, i: usize) -> Value {
        self.get_field(o, i)
    }
    fn set_field_volatile(&self, o: ObjectRef, i: usize, v: Value) {
        self.set_field(o, i, v)
    }
    fn compare_and_swap_field(
        &mut self,
        _o: ObjectRef,
        _i: usize,
        _e: Value,
        _n: Value,
    ) -> bool {
        false
    }
    fn park(&mut self, _t: Option<std::time::Duration>) {}
    fn unpark(&self, _o: ObjectRef) {}
    fn allocate_instance(&mut self, _c: &str) -> Option<ObjectRef> {
        None
    }
    fn class_annotations(&self, _c: ClassId) -> Vec<AnnotationData> {
        Vec::new()
    }
    fn method_annotations(&self, _c: ClassId, _m: &str, _d: &str) -> Vec<AnnotationData> {
        Vec::new()
    }
    fn field_annotations(&self, _c: ClassId, _f: &str) -> Vec<AnnotationData> {
        Vec::new()
    }
    fn method_parameter_annotations(
        &self,
        _c: ClassId,
        _m: &str,
        _d: &str,
    ) -> Vec<Vec<AnnotationData>> {
        Vec::new()
    }
    fn class_signature(&self, _c: ClassId) -> Option<String> {
        None
    }
    fn method_signature(&self, _c: ClassId, _m: &str, _d: &str) -> Option<String> {
        None
    }
    fn field_signature(&self, _c: ClassId, _f: &str) -> Option<String> {
        None
    }
    fn method_annotation_default(
        &self,
        _c: ClassId,
        _m: &str,
        _d: &str,
    ) -> Option<AnnotationElementValue> {
        None
    }
    fn invoke_virtual(
        &mut self,
        _r: ObjectRef,
        _m: &str,
        _d: &str,
        _a: &[Value],
    ) -> MethodCallResult {
        Ok(None)
    }
    fn get_scoped_value(&self, _k: u64) -> Option<Value> {
        None
    }
    fn push_scoped_value(&mut self, _k: u64, _v: Value) {}
    fn pop_scoped_value(&mut self) {}
    fn scoped_value_depth(&self) -> usize {
        0
    }
    fn allocate_native_memory(&mut self, _s: usize, _a: usize) -> Option<(i64, *mut u8)> {
        None
    }
    fn free_native_memory(&mut self, _a: i64) {}
    fn load_native_library(&mut self, _p: &str) -> Result<i64, MethodCallFailed> {
        Ok(0)
    }
    fn find_native_symbol(&self, _l: i64, _n: &str) -> Option<usize> {
        None
    }
    fn register_upcall(&mut self, _e: cratonvm_native_api::ffi::UpcallEntry) -> usize {
        0
    }
    fn get_upcall_info(&self, _s: usize) -> Option<(ObjectRef, Vec<i32>, i32)> {
        None
    }
    fn module_name_of_class(&self, _c: ClassId) -> Option<String> {
        None
    }
    fn find_resource(&self, _n: &str) -> Option<Vec<u8>> {
        None
    }
    fn list_application_class_names(&self) -> Vec<String> {
        Vec::new()
    }
    fn register_dynamic_classpath(&mut self, _p: &[String]) {}
    fn define_class_from_bytes(&mut self, _n: &str, _b: &[u8]) -> Option<ClassId> {
        None
    }
    fn define_class_with_loader(
        &mut self,
        _n: &str,
        _b: &[u8],
        _l: u32,
    ) -> Option<ClassId> {
        None
    }
    fn class_id_by_name_and_loader(&self, _n: &str, _l: u32) -> Option<ClassId> {
        None
    }
    fn allocate_loader_id(&mut self) -> u32 {
        0
    }
    fn discover_reference(
        &mut self,
        _t: u8,
        _r: ObjectRef,
        _f: ObjectRef,
        _q: Option<ObjectRef>,
    ) {
    }
    fn is_package_exported_unqualified(&self, _m: &str, _p: &str) -> bool {
        true
    }
    fn is_package_exported_to(&self, _m: &str, _p: &str, _t: &str) -> bool {
        true
    }
    fn is_package_open_unqualified(&self, _m: &str, _p: &str) -> bool {
        true
    }
    fn is_package_open_to(&self, _m: &str, _p: &str, _t: &str) -> bool {
        true
    }
    fn check_deep_reflection_access(
        &self,
        _a: ClassId,
        _t: ClassId,
    ) -> Result<(), String> {
        Ok(())
    }
}
