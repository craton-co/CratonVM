// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round-5 CRIT regression test: `NativeContext::atomic_fetch_add_int` and
//! `_long` default impls must surface a field-type mismatch as
//! `Err(MethodCallFailed)` wrapping `RuntimeError::IllegalArgumentException`,
//! never silently corrupt the slot's type tag with `Value::Int(delta)` or
//! `Value::Long(delta)`.
//!
//! Before round-5 the default impls' fall-through arm read `_ => 0`, which
//! coerced the wrong-typed slot's previous value to zero, then CAS-wrote a
//! freshly-tagged `Value::Int` / `Value::Long` over it.  A Long field
//! routed through `atomic_fetch_add_int` would emerge re-tagged as Int,
//! permanently corrupting the heap slot's type discriminator (and breaking
//! any GC scanner or descriptor-aware accessor downstream).  The round-5
//! fix routes the mismatch through a catchable Java exception instead.
//!
//! This test drives both default impls against a minimal in-test
//! `NativeContext` whose field store accepts arbitrary `Value` variants —
//! enough to seed a `Long` slot, dispatch the int-variant accessor, and
//! observe the resulting `Err`.
//!
//! Native-api gap §2-2 from `.claude/review-2026-05-24/native-api.md`.

// The in-test mock and the test cases that drive it.  The mock is the
// `MockNativeContext` from `native-api/src/test_mock.rs` (dev branch),
// inlined here because this worktree is built from an older commit that
// predates the `test-mock` feature flag.  Trait surface is the same;
// what's expressed here is the smallest impl that lets us reach
// `atomic_fetch_add_int` / `_long` through the default CAS loop.

mod mock {
    use std::cell::UnsafeCell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
    use std::sync::OnceLock;

    use cratonvm_native_api::ffi::UpcallEntry;
    use cratonvm_native_api::NativeHeapAccess;
    use cratonvm_native_api::{
        AnnotationData, AnnotationElementValue, FieldMetadata, MethodMetadata, NativeContext,
        StackTraceEntry,
    };
    use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
    use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

    type FieldKey = (usize, String);

    pub struct MockNativeContext {
        fields: UnsafeCell<HashMap<FieldKey, Value>>,
        identity_hashes: UnsafeCell<HashMap<usize, i32>>,
        next_hash: AtomicI32,
        next_ptr: AtomicUsize,
        invoke_virtual_result: UnsafeCell<Option<MethodCallResult>>,
    }

    impl Default for MockNativeContext {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockNativeContext {
        pub fn new() -> Self {
            Self {
                fields: UnsafeCell::new(HashMap::new()),
                identity_hashes: UnsafeCell::new(HashMap::new()),
                next_hash: AtomicI32::new(1),
                next_ptr: AtomicUsize::new(8),
                invoke_virtual_result: UnsafeCell::new(None),
            }
        }

        pub fn fresh_object_ref(&self) -> ObjectRef {
            let p = self.next_ptr.fetch_add(8, Ordering::Relaxed);
            // SAFETY: `p >= 8` and a multiple of 8.
            unsafe { ObjectRef::from_raw(p as *mut u8) }
        }

        fn fields_mut(&self) -> &mut HashMap<FieldKey, Value> {
            // SAFETY: single-threaded test code.
            unsafe { &mut *self.fields.get() }
        }
        fn fields_ref(&self) -> &HashMap<FieldKey, Value> {
            unsafe { &*self.fields.get() }
        }
    }

    fn shared_fd_table() -> &'static cratonvm_native_api::fd_table::FileDescriptorTable {
        static TABLE: OnceLock<cratonvm_native_api::fd_table::FileDescriptorTable> =
            OnceLock::new();
        TABLE.get_or_init(cratonvm_native_api::fd_table::FileDescriptorTable::new)
    }

    impl cratonvm_native_api::NativeClassAccess for MockNativeContext {
        fn load_class(&mut self, _n: &str) -> MethodCallResult {
            Ok(None)
        }
        fn class_name_of_id(&self, _c: ClassId) -> Option<String> {
            None
        }
        fn class_id_of_object(&self, _o: ObjectRef) -> ClassId {
            ClassId::new(0)
        }
        fn method_exists(&self, _c: &str, _m: &str, _d: &str) -> bool {
            false
        }
        fn ensure_class_initialized(&mut self, _n: &str) -> Result<ClassId, MethodCallFailed> {
            Ok(ClassId::new(0))
        }

        fn is_subclass(&self, c: ClassId, p: ClassId) -> bool {
            c == p
        }
        fn superclass_of(&self, _c: ClassId) -> Option<ClassId> {
            None
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
        fn primitive_class_mirror(&mut self, _n: &str) -> ObjectRef {
            self.fresh_object_ref()
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
        fn define_class_with_loader(&mut self, _n: &str, _b: &[u8], _l: u32) -> Option<ClassId> {
            None
        }
        fn class_id_by_name_and_loader(&self, _n: &str, _l: u32) -> Option<ClassId> {
            None
        }
        fn allocate_loader_id(&mut self) -> u32 {
            0
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
            _accessor: ClassId,
            _target: ClassId,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    impl cratonvm_native_api::NativeInvokeAccess for MockNativeContext {
        fn invoke(&mut self, _c: &str, _m: &str, _d: &str, _a: &[Value]) -> MethodCallResult {
            Ok(None)
        }

        fn invoke_virtual(
            &mut self,
            _receiver: ObjectRef,
            _method_name: &str,
            _descriptor: &str,
            _args: &[Value],
        ) -> MethodCallResult {
            let slot = unsafe { &mut *self.invoke_virtual_result.get() };
            if let Some(scripted) = slot.take() {
                scripted
            } else {
                Ok(None)
            }
        }
    }

    impl cratonvm_native_api::NativeHeapAccess for MockNativeContext {
        fn new_object(&mut self, _c: &str) -> MethodCallResult {
            Ok(Some(Value::Object(Some(self.fresh_object_ref()))))
        }

        fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
            let key = obj.as_ptr() as usize;
            let map = unsafe { &mut *self.identity_hashes.get() };
            *map.entry(key)
                .or_insert_with(|| self.next_hash.fetch_add(1, Ordering::Relaxed))
        }

        fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
            let key = (obj.as_ptr() as usize, format!("#{index}"));
            self.fields_ref()
                .get(&key)
                .copied()
                .unwrap_or(Value::Int(0))
        }
        fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
            let key = (obj.as_ptr() as usize, format!("#{index}"));
            self.fields_mut().insert(key, value);
        }
        fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value {
            let key = (obj.as_ptr() as usize, field_name.to_string());
            self.fields_ref()
                .get(&key)
                .copied()
                .unwrap_or(Value::Object(None))
        }
        fn set_field_by_name(&self, obj: ObjectRef, field_name: &str, value: Value) {
            let key = (obj.as_ptr() as usize, field_name.to_string());
            self.fields_mut().insert(key, value);
        }
        fn resolve_field_index(&self, _c: &str, _f: &str) -> Option<usize> {
            None
        }
        fn resolve_field_index_by_class_id(&self, _c: ClassId, _f: &str) -> Option<usize> {
            None
        }

        fn new_array(&mut self, _et: ArrayElementType, _length: usize) -> ObjectRef {
            self.fresh_object_ref()
        }
        fn new_ref_array(&mut self, _c: ClassId, _length: usize) -> ObjectRef {
            self.fresh_object_ref()
        }
        fn array_length(&self, _o: ObjectRef) -> usize {
            0
        }
        fn get_array_element(&self, _o: ObjectRef, _i: usize) -> Value {
            Value::Int(0)
        }
        fn set_array_element(&self, _o: ObjectRef, _i: usize, _v: Value) {}

        fn heap_kind_of(&self, _o: ObjectRef) -> ObjectKind {
            ObjectKind::Object
        }
        fn heap_element_type_of(&self, _o: ObjectRef) -> ArrayElementType {
            ArrayElementType::Reference
        }

        fn create_string(&mut self, _t: &str) -> ObjectRef {
            self.fresh_object_ref()
        }
        fn read_string(&self, _o: ObjectRef) -> Option<String> {
            None
        }
        fn get_class_mirror(&mut self, _c: ClassId) -> ObjectRef {
            self.fresh_object_ref()
        }

        fn alloc_object(&mut self, _c: ClassId, _num_fields: usize) -> ObjectRef {
            self.fresh_object_ref()
        }
        fn object_num_fields(&self, _obj: ObjectRef) -> usize {
            0
        }

        fn heap_allocated_bytes(&self) -> usize {
            0
        }

        // The core of this mock: volatile/CAS routes through the plain field
        // store.  The default `atomic_fetch_add_int/_long` impls in the trait
        // are CAS retry loops built atop these — they exercise the type-
        // mismatch fall-through arm exclusively through these three methods.
        fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
            self.get_field(obj, index)
        }
        fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
            self.set_field(obj, index, value)
        }
        fn compare_and_swap_field(
            &mut self,
            obj: ObjectRef,
            index: usize,
            expected: Value,
            new_val: Value,
        ) -> bool {
            let current = self.get_field(obj, index);
            if current == expected {
                self.set_field(obj, index, new_val);
                true
            } else {
                false
            }
        }
        fn allocate_instance(&mut self, _c: &str) -> Option<ObjectRef> {
            Some(self.fresh_object_ref())
        }
        fn discover_reference(
            &mut self,
            _t: u8,
            _r: ObjectRef,
            _f: ObjectRef,
            _q: Option<ObjectRef>,
        ) {
        }
    }

    impl cratonvm_native_api::NativeThreadAccess for MockNativeContext {
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
            self.fresh_object_ref()
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

        fn park(&mut self, _t: Option<std::time::Duration>) {}
        fn unpark(&self, _o: ObjectRef) {}

        fn get_scoped_value(&self, _k: u64) -> Option<Value> {
            None
        }
        fn push_scoped_value(&mut self, _k: u64, _v: Value) {}
        fn pop_scoped_value(&mut self) {}
        fn scoped_value_depth(&self) -> usize {
            0
        }
    }

    impl cratonvm_native_api::NativeExceptionAccess for MockNativeContext {
        fn capture_stack_trace(&mut self, _h: i32) -> Vec<StackTraceEntry> {
            Vec::new()
        }
        fn get_stack_trace(&self, _h: i32) -> Option<Vec<StackTraceEntry>> {
            None
        }
    }

    impl cratonvm_native_api::NativeGpuAccess for MockNativeContext {}

    impl cratonvm_native_api::NativeSystemAccess for MockNativeContext {
        fn record_printed_value(&mut self, _v: Value) {}
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
        fn is_interface_class(&self, _c: ClassId) -> bool {
            false
        }
        fn loaded_class_count(&self) -> usize {
            0
        }
        fn gc_collection_count(&self) -> u64 {
            0
        }
        fn force_gc(&mut self) {}
        fn get_static_field(&self, _c: ClassId, _i: usize) -> Value {
            Value::Int(0)
        }
        fn set_static_field(&mut self, _c: ClassId, _i: usize, _v: Value) {}

        fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
            shared_fd_table()
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
        fn register_upcall(&mut self, _e: UpcallEntry) -> usize {
            0
        }
        fn get_upcall_info(&self, _s: usize) -> Option<(ObjectRef, Vec<i32>, i32)> {
            None
        }
    }

    // GPU offload (`gpu_dispatch_method`, `gpu_future_*`,
    // `gpu_array_download_if_dirty`, `gpu_resolve_lambda_target`,
    // `gpu_release_array_cache`, `gpu_clear_input_cache`) inherits the
    // trait's `None` / no-op defaults — no override needed for the
    // mock's narrow CAS-loop brief.
}

use cratonvm_native_api::NativeContext;
use cratonvm_native_api::NativeHeapAccess;
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
use cratonvm_types::{ClassId, Value};

use mock::MockNativeContext;

/// Helper: assert the error is `IllegalArgumentException` carrying a message
/// that mentions the slot index and the bad value.  Two channels are checked
/// because the default impl threads the message through `format!("…field {} on
/// object is not Int: {:?}", index, other)`.
fn assert_illegal_argument(err: MethodCallFailed, expected_index: usize, expected_type_word: &str) {
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        )) => {
            assert!(
                message.contains(&format!("field {expected_index}")),
                "expected message to mention slot index {expected_index}, got: {message}"
            );
            assert!(
                message.contains(expected_type_word),
                "expected message to mention expected type {expected_type_word:?}, got: {message}"
            );
        }
        other => panic!(
            "expected MethodCallFailed::InternalError(Runtime(IllegalArgumentException)), \
             got: {other:?}"
        ),
    }
}

#[test]
fn atomic_fetch_add_int_rejects_long_slot_without_type_corruption() {
    // Seed slot 0 with a Long value.  Routing `atomic_fetch_add_int`
    // against it MUST surface IllegalArgumentException and MUST NOT
    // re-write the slot.
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(0), 1);
    let seed = Value::Long(0xDEAD_BEEF_CAFE_BABE_u64 as i64);
    ctx.set_field(obj, 0, seed);

    let err = ctx
        .atomic_fetch_add_int(obj, 0, 5)
        .expect_err("atomic_fetch_add_int on a Long slot must return Err");
    assert_illegal_argument(err, 0, "Int");

    // Critical post-condition: the seeded Long is still there, with its
    // original bit pattern.  This is the round-5 type-corruption fix:
    // the prior default impl would have CAS-written `Value::Int(5)` over
    // it, permanently mistyping the heap slot.
    assert_eq!(
        ctx.get_field(obj, 0),
        seed,
        "atomic_fetch_add_int Err path must NOT mutate the slot — \
         this regression-tests the round-5 type-corruption fix"
    );
}

#[test]
fn atomic_fetch_add_long_rejects_int_slot_without_type_corruption() {
    // Mirror test for the Long-variant accessor against an Int slot.
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(0), 1);
    let seed = Value::Int(1234);
    ctx.set_field(obj, 0, seed);

    let err = ctx
        .atomic_fetch_add_long(obj, 0, 99)
        .expect_err("atomic_fetch_add_long on an Int slot must return Err");
    assert_illegal_argument(err, 0, "Long");

    assert_eq!(
        ctx.get_field(obj, 0),
        seed,
        "atomic_fetch_add_long Err path must NOT mutate the slot"
    );
}

#[test]
fn atomic_fetch_add_int_rejects_reference_slot() {
    // A reference slot (Value::Object(_)) is the most dangerous mismatch
    // — silently rewriting it as Int would publish an integer through a
    // reference accessor, and GC root scanning would later try to
    // dereference the int as a heap pointer.  The Err path forbids that.
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(0), 1);
    let referent = ctx.fresh_object_ref();
    let seed = Value::Object(Some(referent));
    ctx.set_field(obj, 0, seed);

    let err = ctx
        .atomic_fetch_add_int(obj, 0, 7)
        .expect_err("atomic_fetch_add_int on a Reference slot must return Err");
    assert_illegal_argument(err, 0, "Int");

    assert_eq!(
        ctx.get_field(obj, 0),
        seed,
        "Reference slot must remain unchanged under the int-accessor Err path"
    );
}

#[test]
fn atomic_fetch_add_long_rejects_reference_slot() {
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(0), 1);
    let referent = ctx.fresh_object_ref();
    let seed = Value::Object(Some(referent));
    ctx.set_field(obj, 0, seed);

    let err = ctx
        .atomic_fetch_add_long(obj, 0, 7)
        .expect_err("atomic_fetch_add_long on a Reference slot must return Err");
    assert_illegal_argument(err, 0, "Long");

    assert_eq!(
        ctx.get_field(obj, 0),
        seed,
        "Reference slot must remain unchanged under the long-accessor Err path"
    );
}

#[test]
fn atomic_fetch_add_int_well_typed_path_still_succeeds() {
    // Sanity check: the Err-path tests above must not have broken the
    // well-typed path that the round-5 fix preserved.  The default
    // impl on an Int slot returns Ok(prev) and bumps the slot.
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(0), 1);
    ctx.set_field(obj, 0, Value::Int(10));

    let prev = ctx
        .atomic_fetch_add_int(obj, 0, 5)
        .expect("well-typed Int CAS must succeed");
    assert_eq!(prev, 10);
    assert_eq!(ctx.get_field(obj, 0), Value::Int(15));
}

#[test]
fn atomic_fetch_add_long_well_typed_path_still_succeeds() {
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(0), 1);
    ctx.set_field(obj, 0, Value::Long(1_000));

    let prev = ctx
        .atomic_fetch_add_long(obj, 0, 250)
        .expect("well-typed Long CAS must succeed");
    assert_eq!(prev, 1_000);
    assert_eq!(ctx.get_field(obj, 0), Value::Long(1_250));
}
