// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Minimal `NativeContext` mock for RA.3 unit tests.
//!
//! Implements only the heap primitives (`new_array`, `array_length`,
//! `get_array_element`, `set_array_element`) and a scripted
//! `invoke_virtual` — enough to exercise `native_reader_read_charbuffer`
//! without standing up the full VM.

#![cfg(test)]

use std::cell::UnsafeCell;
use std::collections::HashMap;

use cratonvm_native_api::{
    AnnotationData, AnnotationElementValue, FieldMetadata, MethodMetadata, NativeContext,
    StackTraceEntry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

/// A recorded `invoke_virtual` call.
#[derive(Debug, Clone)]
pub(crate) struct InvokeCall {
    pub method_name: String,
    pub descriptor: String,
    pub args: Vec<Value>,
}

/// Scripted response for `invoke_virtual` — matched by `(method, descriptor)`.
pub(crate) struct InvokeScript {
    pub method_name: String,
    pub descriptor: String,
    pub result: MethodCallResult,
}

enum HeapEntry {
    Object { fields: Vec<Value> },
    Array { elements: Vec<Value> },
}

pub(crate) struct MockNativeContext {
    heap: UnsafeCell<Vec<HeapEntry>>,
    ptr_to_index: UnsafeCell<HashMap<usize, usize>>,
    next_ptr: usize,
    pub scripts: Vec<InvokeScript>,
    pub calls: UnsafeCell<Vec<InvokeCall>>,
}

impl MockNativeContext {
    pub(crate) fn new() -> Self {
        Self {
            heap: UnsafeCell::new(Vec::new()),
            ptr_to_index: UnsafeCell::new(HashMap::new()),
            next_ptr: 8,
            scripts: Vec::new(),
            calls: UnsafeCell::new(Vec::new()),
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

    fn alloc_entry(&mut self, entry: HeapEntry) -> ObjectRef {
        let idx = self.heap_mut().len();
        self.heap_mut().push(entry);
        let ptr = self.next_ptr;
        self.next_ptr += 8;
        self.ptr_map_mut().insert(ptr, idx);
        unsafe { ObjectRef::from_raw(ptr as *mut u8) }
    }

    fn entry_index(&self, obj: ObjectRef) -> usize {
        let ptr = obj.as_ptr() as usize;
        *self.ptr_map_ref().get(&ptr).expect("invalid ObjectRef")
    }

    pub(crate) fn alloc_object(&mut self, num_fields: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Object {
            fields: vec![Value::Int(0); num_fields],
        })
    }

    pub(crate) fn script(&mut self, method: &str, desc: &str, result: MethodCallResult) {
        self.scripts.push(InvokeScript {
            method_name: method.to_string(),
            descriptor: desc.to_string(),
            result,
        });
    }

    pub(crate) fn recorded_calls(&self) -> &[InvokeCall] {
        unsafe { &*self.calls.get() }
    }
}

impl NativeContext for MockNativeContext {
    // --- minimal heap primitives used by the native under test ---
    fn new_array(&mut self, _et: ArrayElementType, length: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Array {
            elements: vec![Value::Int(0); length],
        })
    }
    fn array_length(&self, obj: ObjectRef) -> usize {
        match &self.heap_ref()[self.entry_index(obj)] {
            HeapEntry::Array { elements } => elements.len(),
            _ => 0,
        }
    }
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
        match &self.heap_ref()[self.entry_index(obj)] {
            HeapEntry::Array { elements } => elements.get(index).copied().unwrap_or(Value::Int(0)),
            _ => Value::Int(0),
        }
    }
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
        let idx = self.entry_index(obj);
        if let HeapEntry::Array { elements } = &mut self.heap_mut()[idx] {
            if index < elements.len() {
                elements[index] = value;
            }
        }
    }
    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        match &self.heap_ref()[self.entry_index(obj)] {
            HeapEntry::Object { fields } => fields.get(index).copied().unwrap_or(Value::Int(0)),
            _ => Value::Int(0),
        }
    }
    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        let idx = self.entry_index(obj);
        if let HeapEntry::Object { fields } = &mut self.heap_mut()[idx] {
            if index >= fields.len() {
                fields.resize(index + 1, Value::Int(0));
            }
            fields[index] = value;
        }
    }

    // --- JPMS module-access checks: classpath-only mock, permissive ---
    fn is_package_exported_unqualified(&self, _module_name: &str, _pkg: &str) -> bool {
        true
    }
    fn is_package_exported_to(&self, _module_name: &str, _pkg: &str, _to_module: &str) -> bool {
        true
    }
    fn is_package_open_unqualified(&self, _module_name: &str, _pkg: &str) -> bool {
        true
    }
    fn is_package_open_to(&self, _module_name: &str, _pkg: &str, _to_module: &str) -> bool {
        true
    }
    fn check_deep_reflection_access(
        &self,
        _accessor_class_id: ClassId,
        _target_class_id: ClassId,
    ) -> Result<(), String> {
        Ok(())
    }

    fn invoke_virtual(
        &mut self,
        _receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // Record the call so tests can assert on it.
        let calls = unsafe { &mut *self.calls.get() };
        calls.push(InvokeCall {
            method_name: method_name.to_string(),
            descriptor: descriptor.to_string(),
            args: args.to_vec(),
        });
        // Find the first matching script (FIFO per key).
        if let Some(pos) = self
            .scripts
            .iter()
            .position(|s| s.method_name == method_name && s.descriptor == descriptor)
        {
            return self.scripts.remove(pos).result;
        }
        Ok(None)
    }

    // --- everything else: default stubs (unused by the test) ---
    fn load_class(&mut self, _n: &str) -> MethodCallResult { Ok(None) }
    fn new_object(&mut self, _c: &str) -> MethodCallResult { Ok(None) }
    fn invoke(&mut self, _c: &str, _m: &str, _d: &str, _a: &[Value]) -> MethodCallResult { Ok(None) }
    fn identity_hash_code(&self, o: ObjectRef) -> i32 { o.as_ptr() as i32 }
    fn record_printed_value(&mut self, _v: Value) {}
    fn class_name_of_id(&self, _c: ClassId) -> Option<String> { None }
    fn class_id_of_object(&self, _o: ObjectRef) -> ClassId { ClassId::new(0) }
    fn capture_stack_trace(&mut self, _h: i32) -> Vec<StackTraceEntry> { Vec::new() }
    fn get_stack_trace(&self, _h: i32) -> Option<&[StackTraceEntry]> { None }
    fn get_field_by_name(&self, _o: ObjectRef, _n: &str) -> Value { Value::Object(None) }
    fn set_field_by_name(&self, _o: ObjectRef, _n: &str, _v: Value) {}
    fn resolve_field_index(&self, _c: &str, _f: &str) -> Option<usize> { None }
    fn method_exists(&self, _c: &str, _m: &str, _d: &str) -> bool { false }
    fn new_ref_array(&mut self, _c: ClassId, length: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Array {
            elements: vec![Value::Object(None); length],
        })
    }
    fn heap_kind_of(&self, _o: ObjectRef) -> ObjectKind { ObjectKind::Object }
    fn heap_element_type_of(&self, _o: ObjectRef) -> ArrayElementType { ArrayElementType::Reference }
    fn create_string(&mut self, _t: &str) -> ObjectRef { self.alloc_object(0) }
    fn read_string(&self, _o: ObjectRef) -> Option<String> { None }
    fn get_class_mirror(&mut self, _c: ClassId) -> ObjectRef { self.alloc_object(0) }
    fn record_printed_line(&mut self, _t: String) {}
    fn get_system_stream(&self, _n: &str) -> Option<ObjectRef> { None }
    fn get_system_property(&self, _k: &str) -> Option<String> { None }
    fn set_system_property(&mut self, _k: &str, _v: &str) -> Option<String> { None }
    fn alloc_object(&mut self, _c: ClassId, num_fields: usize) -> ObjectRef {
        MockNativeContext::alloc_object(self, num_fields)
    }
    fn ensure_class_initialized(&mut self, _n: &str) -> Result<ClassId, MethodCallFailed> {
        Ok(ClassId::new(0))
    }
    fn is_subclass(&self, _c: ClassId, _p: ClassId) -> bool { false }
    fn superclass_of(&self, _c: ClassId) -> Option<ClassId> { None }
    fn is_interface_class(&self, _c: ClassId) -> bool { false }
    fn class_id_by_name(&self, _n: &str) -> Option<ClassId> { None }
    fn loader_id_of_class(&self, _c: ClassId) -> i32 { 2 }
    fn is_record_class(&self, _c: ClassId) -> bool { false }
    fn record_components(&self, _c: ClassId) -> Vec<(String, String)> { Vec::new() }
    fn is_sealed_class(&self, _c: ClassId) -> bool { false }
    fn permitted_subclasses(&self, _c: ClassId) -> Vec<String> { Vec::new() }
    fn object_num_fields(&self, obj: ObjectRef) -> usize {
        match &self.heap_ref()[self.entry_index(obj)] {
            HeapEntry::Object { fields } => fields.len(),
            _ => 0,
        }
    }
    fn thread_id(&self) -> u64 { 1 }
    fn monitor_enter(&mut self, _o: ObjectRef) {}
    fn monitor_exit(&mut self, _o: ObjectRef) {}
    fn monitor_wait(&mut self, _o: ObjectRef, _t: Option<u64>) -> MethodCallResult { Ok(None) }
    fn monitor_notify(&mut self, _o: ObjectRef) -> MethodCallResult { Ok(None) }
    fn monitor_notify_all(&mut self, _o: ObjectRef) -> MethodCallResult { Ok(None) }
    fn thread_start(&mut self, _o: ObjectRef) -> MethodCallResult { Ok(None) }
    fn thread_join(&mut self, _o: ObjectRef) -> MethodCallResult { Ok(None) }
    fn thread_is_alive(&self, _o: ObjectRef) -> bool { false }
    fn current_thread_object(&mut self) -> ObjectRef { self.alloc_object(0) }
    fn thread_interrupt(&mut self, _o: ObjectRef) {}
    fn is_interrupted(&self, _c: bool) -> bool { false }
    fn active_thread_count(&self) -> i32 { 1 }
    fn enumerate_threads(&self, _m: usize) -> Vec<ObjectRef> { Vec::new() }
    fn heap_allocated_bytes(&self) -> usize { 0 }
    fn loaded_class_count(&self) -> usize { 0 }
    fn gc_collection_count(&self) -> u64 { 0 }
    fn force_gc(&mut self) {}
    fn declared_fields(&self, _c: ClassId) -> Vec<FieldMetadata> { Vec::new() }
    fn declared_methods(&self, _c: ClassId) -> Vec<MethodMetadata> { Vec::new() }
    fn class_interfaces(&self, _c: ClassId) -> Vec<ClassId> { Vec::new() }
    fn class_access_flags(&self, _c: ClassId) -> u16 { 0 }
    fn get_static_field(&self, _c: ClassId, _i: usize) -> Value { Value::Int(0) }
    fn set_static_field(&mut self, _c: ClassId, _i: usize, _v: Value) {}
    fn primitive_class_mirror(&mut self, _n: &str) -> ObjectRef { self.alloc_object(0) }
    fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
        use std::sync::OnceLock;
        static FD: OnceLock<cratonvm_native_api::fd_table::FileDescriptorTable> = OnceLock::new();
        FD.get_or_init(cratonvm_native_api::fd_table::FileDescriptorTable::new)
    }
    fn get_field_volatile(&self, o: ObjectRef, i: usize) -> Value { self.get_field(o, i) }
    fn set_field_volatile(&self, o: ObjectRef, i: usize, v: Value) { self.set_field(o, i, v) }
    fn compare_and_swap_field(
        &mut self, _o: ObjectRef, _i: usize, _e: Value, _n: Value,
    ) -> bool { false }
    fn park(&mut self, _t: Option<std::time::Duration>) {}
    fn unpark(&self, _o: ObjectRef) {}
    fn allocate_instance(&mut self, _c: &str) -> Option<ObjectRef> { None }
    fn class_annotations(&self, _c: ClassId) -> Vec<AnnotationData> { Vec::new() }
    fn method_annotations(
        &self, _c: ClassId, _m: &str, _d: &str,
    ) -> Vec<AnnotationData> { Vec::new() }
    fn field_annotations(&self, _c: ClassId, _f: &str) -> Vec<AnnotationData> { Vec::new() }
    fn method_parameter_annotations(
        &self, _c: ClassId, _m: &str, _d: &str,
    ) -> Vec<Vec<AnnotationData>> { Vec::new() }
    fn class_signature(&self, _c: ClassId) -> Option<String> { None }
    fn method_signature(
        &self, _c: ClassId, _m: &str, _d: &str,
    ) -> Option<String> { None }
    fn field_signature(&self, _c: ClassId, _f: &str) -> Option<String> { None }
    fn method_annotation_default(
        &self, _c: ClassId, _m: &str, _d: &str,
    ) -> Option<AnnotationElementValue> { None }
    fn get_scoped_value(&self, _k: u64) -> Option<Value> { None }
    fn push_scoped_value(&mut self, _k: u64, _v: Value) {}
    fn pop_scoped_value(&mut self) {}
    fn scoped_value_depth(&self) -> usize { 0 }
    fn allocate_native_memory(&mut self, _s: usize, _a: usize) -> Option<(i64, *mut u8)> { None }
    fn free_native_memory(&mut self, _a: i64) {}
    fn load_native_library(&mut self, _p: &str) -> Result<i64, MethodCallFailed> { Ok(0) }
    fn find_native_symbol(&self, _l: i64, _n: &str) -> Option<usize> { None }
    fn register_upcall(&mut self, _e: cratonvm_native_api::ffi::UpcallEntry) -> usize { 0 }
    fn get_upcall_info(&self, _s: usize) -> Option<(ObjectRef, Vec<i32>, i32)> { None }
    fn module_name_of_class(&self, _c: ClassId) -> Option<String> { None }
    fn find_resource(&self, _n: &str) -> Option<Vec<u8>> { None }
    fn list_application_class_names(&self) -> Vec<String> { Vec::new() }
    fn register_dynamic_classpath(&mut self, _p: &[String]) {}
    fn define_class_from_bytes(&mut self, _n: &str, _b: &[u8]) -> Option<ClassId> { None }
    fn define_class_with_loader(
        &mut self, _n: &str, _b: &[u8], _l: u32,
    ) -> Option<ClassId> { None }
    fn class_id_by_name_and_loader(&self, _n: &str, _l: u32) -> Option<ClassId> { None }
    fn allocate_loader_id(&mut self) -> u32 { 0 }
    fn discover_reference(
        &mut self, _t: u8, _r: ObjectRef, _f: ObjectRef, _q: Option<ObjectRef>,
    ) {}
}
