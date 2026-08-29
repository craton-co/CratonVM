// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Minimal in-crate [`NativeContext`] implementation for trait-default-impl tests.
//!
//! Gated behind `#[cfg(any(test, feature = "test-mock"))]` so the symbol is
//! available to other workspace crates that opt in via
//! `cratonvm-native-api = { ..., features = ["test-mock"] }`, but never
//! enters production builds.
//!
//! ## Scope
//!
//! The trait `NativeContext` has ~150 methods covering the full VM surface
//! (heap, threading, JIT offload, JPMS, reflection, FFI, GC, …). The mocks
//! that actually drive production-shaped behaviour live next to each consumer
//! (e.g. `native-builtins/src/test_utils.rs` and `native-io/src/test_support.rs`).
//!
//! This mock has a narrower brief: it is the smallest impl that lets us
//! **test the default method bodies in [`NativeContext`] itself** — the
//! `default fn` definitions that wrap a CAS loop, a heap probe, etc.
//! Specifically it provides:
//!
//!  * a tiny `HashMap<(ObjectRef, &'static str), Value>` field store —
//!    enough to exercise the `set_field` → `get_field` round-trip and to
//!    drive the default `atomic_fetch_add_int` / `atomic_fetch_add_long`
//!    CAS loops that build on `get_field_volatile` / `set_field_volatile`
//!    / `compare_and_swap_field`,
//!  * an identity-hash-code allocator backed by an `AtomicI32` counter so
//!    every `ObjectRef` gets a distinct, stable hash,
//!  * `alloc_object` / `new_array` stubs returning fresh, 8-byte-aligned
//!    `ObjectRef` pointers, and
//!  * a programmable `invoke_virtual` that returns a caller-configured
//!    [`MethodCallResult`] (defaults to `Ok(None)` — Java `void`), and
//!  * an opt-in class model ([`MockNativeContext::declare_class`]) so
//!    `resolve_field_index` / `resolve_field_index_by_class_id` can answer
//!    something other than a constant `None`. Nothing is declared by default,
//!    so an untouched mock behaves exactly as it always has.
//!
//! ## Fidelity note
//!
//! `get_field_by_name` here answers `Value::Object(None)` for a name it cannot
//! resolve, which is what the production `NativeContextImpl` does
//! (`vm/src/vm/vm_exec.rs:10613-10623`) and what the trait promises
//! (`registry.rs`: "Returns `Value::Object(None)` if the field is not found").
//! `native-builtins/src/test_utils.rs`'s much larger `MockNativeContext`
//! answers `Value::Int(0)` instead — see the DIVERGENCE note on its
//! `get_field_by_name`. Do not carry a conclusion from that mock to this one.
//!
//! Tests that need real heap layout, real class loading, or real threading
//! should keep using `native-builtins`'s `MockNativeContext` instead.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::OnceLock;

use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

use crate::ffi::UpcallEntry;
use crate::registry::{
    AnnotationData, AnnotationElementValue, FieldMetadata, MethodMetadata, NativeClassAccess,
    NativeContext, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess, StackTraceEntry,
};

/// Composite key for the field store.
///
/// Held as a `(usize, String)` so the `&'static str` requirement implied
/// by `set_field_by_name` is not actually inherited (the production API
/// passes `&str`, but the slot it lands in is not bound to a `'static`
/// lifetime). Pointer + owned name keeps the entry self-contained and
/// avoids any lifetime threading through the mock.
type FieldKey = (usize, String);

struct MockArray {
    element_type: ArrayElementType,
    values: Vec<Value>,
}

fn default_array_value(element_type: ArrayElementType) -> Value {
    match element_type {
        ArrayElementType::Boolean
        | ArrayElementType::Byte
        | ArrayElementType::Char
        | ArrayElementType::Short
        | ArrayElementType::Int => Value::Int(0),
        ArrayElementType::Long => Value::Long(0),
        ArrayElementType::Float => Value::Float(0.0),
        ArrayElementType::Double => Value::Double(0.0),
        ArrayElementType::Reference => Value::Object(None),
    }
}

fn value_matches_array_type(element_type: ArrayElementType, value: Value) -> bool {
    match element_type {
        ArrayElementType::Boolean
        | ArrayElementType::Byte
        | ArrayElementType::Char
        | ArrayElementType::Short
        | ArrayElementType::Int => matches!(value, Value::Int(_)),
        ArrayElementType::Long => matches!(value, Value::Long(_)),
        ArrayElementType::Float => matches!(value, Value::Float(_)),
        ArrayElementType::Double => matches!(value, Value::Double(_)),
        ArrayElementType::Reference => matches!(value, Value::Object(_)),
    }
}

/// Minimal NativeContext suitable for exercising trait default impls.
///
/// Single-threaded test code only — the inner `UnsafeCell` is the same
/// pattern used by the other mocks in the workspace (the trait requires
/// `&self` for `set_field` and friends).
pub struct MockNativeContext {
    /// Field store keyed by `(object_pointer, slot_or_name)`.
    fields: UnsafeCell<HashMap<FieldKey, Value>>,
    /// Tiny array backing store keyed by the synthetic object pointer.
    arrays: UnsafeCell<HashMap<usize, MockArray>>,
    /// Per-object identity hash codes. Allocated on first call to
    /// [`identity_hash_code`] for each pointer.
    identity_hashes: UnsafeCell<HashMap<usize, i32>>,
    /// Monotonically incrementing identity-hash counter — guarantees
    /// distinct hashes per object as long as the counter does not wrap
    /// (4 billion objects is well outside any conceivable test).
    next_hash: AtomicI32,
    /// Monotonically incrementing pointer counter. Pointers start at 8
    /// and grow in 8-byte steps so every `ObjectRef` is non-null and
    /// 8-byte aligned — the only invariant the trait demands.
    next_ptr: AtomicUsize,
    /// Configurable result for the next `invoke_virtual` call. Tests
    /// override this via [`set_invoke_virtual_result`] to script the
    /// default-impl callers that route through `invoke_virtual` for
    /// dispatch. `None` means "return `Ok(None)` (Java `void`)".
    invoke_virtual_result: UnsafeCell<Option<MethodCallResult>>,
    /// Collector-visible slots used to exercise `NativeHandleScope`.
    handle_slots: UnsafeCell<Vec<Option<ObjectRef>>>,
    /// Nested scope bases, matching the production `JvmThread` layout.
    handle_scope_bases: UnsafeCell<Vec<usize>>,
    /// Optional class model: the instance fields a test says `ClassId` (as
    /// `u32`) declares. Empty by default, so `resolve_field_index*` and
    /// `declared_fields` answer exactly as they did before this existed.
    /// Populated via [`MockNativeContext::declare_class`].
    declared_fields: UnsafeCell<HashMap<u32, Vec<FieldMetadata>>>,
    /// `class_name -> ClassId` for the classes a test declared, so
    /// `resolve_field_index` (the by-NAME form) can reach the model.
    class_ids_by_name: UnsafeCell<HashMap<String, ClassId>>,
    /// `ClassId -> class_name`, the inverse of `class_ids_by_name`.
    class_names_by_id: UnsafeCell<HashMap<u32, String>>,
    /// `object_pointer -> (ClassId, num_fields)` for objects minted through
    /// `alloc_object`. Absent objects keep the historical answers
    /// (`ClassId::new(0)` / `0` fields).
    object_classes: UnsafeCell<HashMap<usize, (ClassId, usize)>>,
}

impl Default for MockNativeContext {
    fn default() -> Self {
        Self::new()
    }
}

impl MockNativeContext {
    /// Build a fresh mock with an empty heap, hash counter primed at 1,
    /// and pointer counter primed at 8 (the smallest non-null 8-aligned
    /// value).
    pub fn new() -> Self {
        Self {
            fields: UnsafeCell::new(HashMap::new()),
            arrays: UnsafeCell::new(HashMap::new()),
            identity_hashes: UnsafeCell::new(HashMap::new()),
            next_hash: AtomicI32::new(1),
            next_ptr: AtomicUsize::new(8),
            invoke_virtual_result: UnsafeCell::new(None),
            handle_slots: UnsafeCell::new(Vec::new()),
            handle_scope_bases: UnsafeCell::new(Vec::new()),
            declared_fields: UnsafeCell::new(HashMap::new()),
            class_ids_by_name: UnsafeCell::new(HashMap::new()),
            class_names_by_id: UnsafeCell::new(HashMap::new()),
            object_classes: UnsafeCell::new(HashMap::new()),
        }
    }

    /// Register a class with a known instance-field layout and return its
    /// `ClassId`.
    ///
    /// Without this the mock has no class metadata at all, so
    /// `resolve_field_index` / `resolve_field_index_by_class_id` can only ever
    /// answer `None` — and the CLASS-SIDE WITNESS pattern
    /// (`resolve_field_index_by_class_id(class_id, "<a field only the real JDK
    /// class declares>").is_none()`, e.g.
    /// `native-builtins/src/classloader.rs::cl_has_synthetic_layout`) then
    /// takes its synthetic arm unconditionally. A unit test of a dual-layout
    /// discriminator written against such a mock passes vacuously: it never
    /// exercises the real-layout arm, and can be green while production takes
    /// the other one.
    ///
    /// Both arms are reachable now:
    ///
    /// * REAL layout — `declare_class("p/Real", &[("parent", "Ljava/lang/ClassLoader;")])`,
    ///   then `alloc_object(cid, n)`. The witness resolves and the predicate is
    ///   false.
    /// * FABRICATED layout — allocate under a `ClassId` that was never declared
    ///   (or `fresh_object_ref`). The witness answers `None` and the predicate
    ///   is true.
    ///
    /// `ClassId`s are handed out from 1 upward so that `ClassId::new(0)` stays
    /// the "unknown class" answer `class_id_of_object` gives an undeclared
    /// object.
    pub fn declare_class(&self, class_name: &str, fields: &[(&str, &str)]) -> ClassId {
        // SAFETY: single-threaded test code.
        let by_name = unsafe { &mut *self.class_ids_by_name.get() };
        if let Some(&existing) = by_name.get(class_name) {
            return existing;
        }
        let class_id = ClassId::new(u32::try_from(by_name.len() + 1).expect("mock class overflow"));
        by_name.insert(class_name.to_string(), class_id);
        // SAFETY: single-threaded test code.
        unsafe { &mut *self.class_names_by_id.get() }
            .insert(class_id.as_u32(), class_name.to_string());
        let metadata = fields
            .iter()
            .enumerate()
            .map(|(slot_index, (name, descriptor))| FieldMetadata {
                name: (*name).to_string(),
                descriptor: (*descriptor).to_string(),
                access_flags: 0,
                slot_index,
                declaring_class_id: class_id,
                is_static: false,
            })
            .collect();
        // SAFETY: single-threaded test code.
        unsafe { &mut *self.declared_fields.get() }.insert(class_id.as_u32(), metadata);
        class_id
    }

    /// Mint a brand-new heap pointer. Always non-null and 8-byte aligned.
    ///
    /// Public so tests can fabricate `ObjectRef`s for trait methods that
    /// don't go through `alloc_object` (e.g. driving `monitor_enter` on
    /// a synthetic pointer).
    pub fn fresh_object_ref(&self) -> ObjectRef {
        let p = self.next_ptr.fetch_add(8, Ordering::Relaxed);
        // SAFETY: `p >= 8` and a multiple of 8 — meets the documented
        // ObjectRef invariants (non-null, 8-byte aligned).
        unsafe { ObjectRef::from_raw(p as *mut u8) }
    }

    /// Pre-load the result the next `invoke_virtual` call will return.
    /// The script is consumed (single-shot) — call again to re-arm.
    pub fn set_invoke_virtual_result(&self, result: MethodCallResult) {
        // SAFETY: single-threaded test code.
        unsafe { *self.invoke_virtual_result.get() = Some(result) };
    }

    /// Number of currently rooted handle slots.
    pub fn handle_slot_count(&self) -> usize {
        // SAFETY: single-threaded test code.
        unsafe { (&*self.handle_slots.get()).len() }
    }

    /// Number of currently open handle scopes.
    pub fn handle_scope_depth(&self) -> usize {
        // SAFETY: single-threaded test code.
        unsafe { (&*self.handle_scope_bases.get()).len() }
    }

    fn fields_mut(&self) -> &mut HashMap<FieldKey, Value> {
        // SAFETY: single-threaded test code, no aliased references escape.
        unsafe { &mut *self.fields.get() }
    }

    fn fields_ref(&self) -> &HashMap<FieldKey, Value> {
        // SAFETY: single-threaded test code.
        unsafe { &*self.fields.get() }
    }

    fn arrays_mut(&self) -> &mut HashMap<usize, MockArray> {
        // SAFETY: single-threaded test code, no aliased references escape.
        unsafe { &mut *self.arrays.get() }
    }

    fn arrays_ref(&self) -> &HashMap<usize, MockArray> {
        // SAFETY: single-threaded test code.
        unsafe { &*self.arrays.get() }
    }
}

/// Lazily-initialised file descriptor table — most trait methods that
/// touch it are unused by default-impl tests, but `fd_table()` is non-
/// optional, so we return a stable reference to a single global table.
fn shared_fd_table() -> &'static crate::fd_table::FileDescriptorTable {
    static TABLE: OnceLock<crate::fd_table::FileDescriptorTable> = OnceLock::new();
    TABLE.get_or_init(crate::fd_table::FileDescriptorTable::new)
}

impl NativeClassAccess for MockNativeContext {
    // --------------------------------------------------------------
    // Class loading + invocation — no real loader, all stubs.
    // --------------------------------------------------------------

    fn load_class(&mut self, _n: &str) -> MethodCallResult {
        Ok(None)
    }

    fn class_name_of_id(&self, c: ClassId) -> Option<String> {
        // SAFETY: single-threaded test code.
        unsafe { &*self.class_names_by_id.get() }
            .get(&c.as_u32())
            .cloned()
    }
    fn class_id_of_object(&self, o: ObjectRef) -> ClassId {
        // SAFETY: single-threaded test code.
        unsafe { &*self.object_classes.get() }
            .get(&(o.as_ptr() as usize))
            .map_or(ClassId::new(0), |&(class_id, _)| class_id)
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
    fn class_id_by_name(&self, n: &str) -> Option<ClassId> {
        // SAFETY: single-threaded test code.
        unsafe { &*self.class_ids_by_name.get() }.get(n).copied()
    }
    fn loader_id_of_class(&self, _c: ClassId) -> i32 {
        2
    } // Application loader by default
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

    fn declared_fields(&self, c: ClassId) -> Vec<FieldMetadata> {
        // SAFETY: single-threaded test code.
        // `FieldMetadata` is not `Clone`, so rebuild each entry by hand.
        unsafe { &*self.declared_fields.get() }
            .get(&c.as_u32())
            .map(|fields| {
                fields
                    .iter()
                    .map(|meta| FieldMetadata {
                        name: meta.name.clone(),
                        descriptor: meta.descriptor.clone(),
                        access_flags: meta.access_flags,
                        slot_index: meta.slot_index,
                        declaring_class_id: meta.declaring_class_id,
                        is_static: meta.is_static,
                    })
                    .collect()
            })
            .unwrap_or_default()
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

    // JPMS — classpath-only / permissive.
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

impl NativeInvokeAccess for MockNativeContext {
    fn invoke(&mut self, _c: &str, _m: &str, _d: &str, _a: &[Value]) -> MethodCallResult {
        Ok(None)
    }

    // --------------------------------------------------------------
    // `invoke_virtual` — programmable single-shot script. When no
    // script is armed, returns `Ok(None)` (Java `void`).
    // --------------------------------------------------------------

    fn invoke_virtual(
        &mut self,
        _receiver: ObjectRef,
        _method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> MethodCallResult {
        // SAFETY: single-threaded test code.
        let slot = unsafe { &mut *self.invoke_virtual_result.get() };
        if let Some(scripted) = slot.take() {
            scripted
        } else {
            Ok(None)
        }
    }
}

impl NativeHeapAccess for MockNativeContext {
    fn new_object(&mut self, _c: &str) -> MethodCallResult {
        Ok(Some(Value::Object(Some(self.fresh_object_ref()))))
    }

    fn handle_scope_push(&mut self) {
        // SAFETY: single-threaded test code.
        let slots = unsafe { &*self.handle_slots.get() };
        // SAFETY: single-threaded test code.
        unsafe { &mut *self.handle_scope_bases.get() }.push(slots.len());
    }

    fn handle_scope_pop(&mut self) {
        // SAFETY: single-threaded test code.
        let bases = unsafe { &mut *self.handle_scope_bases.get() };
        let Some(base) = bases.pop() else {
            return;
        };
        // SAFETY: single-threaded test code.
        unsafe { &mut *self.handle_slots.get() }.truncate(base);
    }

    fn handle_root(&mut self, object: ObjectRef) -> u32 {
        // SAFETY: single-threaded test code.
        let slots = unsafe { &mut *self.handle_slots.get() };
        let slot = slots.len();
        slots.push(Some(object));
        u32::try_from(slot).expect("mock native handle table exceeded u32")
    }

    fn handle_get(&self, slot: u32) -> Option<ObjectRef> {
        // SAFETY: single-threaded test code.
        unsafe { &*self.handle_slots.get() }
            .get(slot as usize)
            .copied()
            .flatten()
    }

    // --------------------------------------------------------------
    // Identity-hash allocator — distinct per object, cached so the same
    // ObjectRef yields the same hash across calls (System.identityHashCode
    // contract).
    // --------------------------------------------------------------

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        let key = obj.as_ptr() as usize;
        // SAFETY: single-threaded test code.
        let map = unsafe { &mut *self.identity_hashes.get() };
        *map.entry(key)
            .or_insert_with(|| self.next_hash.fetch_add(1, Ordering::Relaxed))
    }

    // --------------------------------------------------------------
    // Field store. Keyed by (object_pointer, slot/name) in the same
    // `HashMap` so the slot-indexed and name-indexed APIs agree on
    // writes by either path.
    // --------------------------------------------------------------

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
    /// Resolve against the class model a test registered with
    /// [`MockNativeContext::declare_class`]. `None` — the answer for every
    /// class before that model existed — now means "this class was never
    /// declared to the mock", which is what makes the class-side-witness
    /// predicate FALSIFIABLE here rather than constant. Matches production
    /// (`vm/src/vm/vm_exec.rs:10637-10650`), which resolves the name in the
    /// hierarchy and answers `None` only when it is genuinely absent.
    fn resolve_field_index(&self, c: &str, f: &str) -> Option<usize> {
        self.resolve_field_index_by_class_id(self.class_id_by_name(c)?, f)
    }
    fn resolve_field_index_by_class_id(&self, c: ClassId, f: &str) -> Option<usize> {
        // SAFETY: single-threaded test code.
        unsafe { &*self.declared_fields.get() }
            .get(&c.as_u32())?
            .iter()
            .find(|meta| !meta.is_static && meta.name == f)
            .map(|meta| meta.slot_index)
    }

    // --------------------------------------------------------------
    // Arrays - minimal backing store for trait default-impl tests.
    // --------------------------------------------------------------

    fn new_array(&mut self, et: ArrayElementType, length: usize) -> ObjectRef {
        let obj = self.fresh_object_ref();
        let default = default_array_value(et);
        self.arrays_mut().insert(
            obj.as_ptr() as usize,
            MockArray {
                element_type: et,
                values: vec![default; length],
            },
        );
        obj
    }
    fn new_ref_array(&mut self, _c: ClassId, length: usize) -> ObjectRef {
        self.new_array(ArrayElementType::Reference, length)
    }
    fn array_length(&self, o: ObjectRef) -> usize {
        self.arrays_ref()
            .get(&(o.as_ptr() as usize))
            .map_or(0, |arr| arr.values.len())
    }
    fn get_array_element(&self, o: ObjectRef, i: usize) -> Value {
        match self.arrays_ref().get(&(o.as_ptr() as usize)) {
            Some(arr) => arr
                .values
                .get(i)
                .copied()
                .unwrap_or_else(|| default_array_value(arr.element_type)),
            None => Value::Int(0),
        }
    }
    fn set_array_element(&self, o: ObjectRef, i: usize, v: Value) {
        if let Some(arr) = self.arrays_mut().get_mut(&(o.as_ptr() as usize)) {
            if i < arr.values.len() && value_matches_array_type(arr.element_type, v) {
                arr.values[i] = v;
            }
        }
    }

    fn heap_kind_of(&self, o: ObjectRef) -> ObjectKind {
        if self.arrays_ref().contains_key(&(o.as_ptr() as usize)) {
            ObjectKind::Array
        } else {
            ObjectKind::Object
        }
    }
    fn heap_element_type_of(&self, o: ObjectRef) -> ArrayElementType {
        self.arrays_ref()
            .get(&(o.as_ptr() as usize))
            .map_or(ArrayElementType::Reference, |arr| arr.element_type)
    }

    // --------------------------------------------------------------
    // Strings — minimal, opaque heap objects.
    // --------------------------------------------------------------

    fn create_string(&mut self, _t: &str) -> ObjectRef {
        self.fresh_object_ref()
    }
    fn read_string(&self, _o: ObjectRef) -> Option<String> {
        None
    }

    fn get_class_mirror(&mut self, _c: ClassId) -> ObjectRef {
        self.fresh_object_ref()
    }

    fn alloc_object(&mut self, c: ClassId, num_fields: usize) -> ObjectRef {
        // The field store grows on demand, so the count does not bound writes.
        // Both are recorded anyway so `class_id_of_object` /
        // `object_num_fields` can answer for this object — the class-side
        // witness needs the receiver's `ClassId` to be its own, not a constant.
        let obj = self.fresh_object_ref();
        // SAFETY: single-threaded test code.
        unsafe { &mut *self.object_classes.get() }.insert(obj.as_ptr() as usize, (c, num_fields));
        obj
    }
    fn object_num_fields(&self, obj: ObjectRef) -> usize {
        // SAFETY: single-threaded test code.
        unsafe { &*self.object_classes.get() }
            .get(&(obj.as_ptr() as usize))
            .map_or(0, |&(_, num_fields)| num_fields)
    }

    fn heap_allocated_bytes(&self) -> usize {
        0
    }

    // --------------------------------------------------------------
    // Volatile + CAS — back onto the plain field store. Sufficient for
    // the default-impl `atomic_fetch_add_int`/`_long` CAS loops.
    // --------------------------------------------------------------

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
    fn discover_reference(&mut self, _t: u8, _r: ObjectRef, _f: ObjectRef, _q: Option<ObjectRef>) {}
}

impl NativeThreadAccess for MockNativeContext {
    // --------------------------------------------------------------
    // Threading — single-threaded stubs.
    // --------------------------------------------------------------

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

    // --------------------------------------------------------------
    // Scoped values + Panama FFI — inert.
    // --------------------------------------------------------------

    fn get_scoped_value(&self, _k: u64) -> Option<Value> {
        None
    }
    fn push_scoped_value(&mut self, _k: u64, _v: Value) {}
    fn pop_scoped_value(&mut self) {}
    fn scoped_value_depth(&self) -> usize {
        0
    }
}

impl NativeExceptionAccess for MockNativeContext {
    fn capture_stack_trace(&mut self, _h: i32) -> Vec<StackTraceEntry> {
        Vec::new()
    }
    fn get_stack_trace(&self, _h: i32) -> Option<Vec<StackTraceEntry>> {
        None
    }
}

impl NativeGpuAccess for MockNativeContext {}

impl NativeSystemAccess for MockNativeContext {
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

    fn fd_table(&self) -> &crate::fd_table::FileDescriptorTable {
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

// `Send` is auto-derived (every field is `Send`). We deliberately do NOT
// implement `Sync` — the `UnsafeCell` interiors would race under any real
// shared use, and the mock is single-threaded by contract.

// --------------------------------------------------------------------
// Self-tests — exercise the mock itself so consumers can rely on it.
// --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_round_trip_via_slot_index() {
        let mut ctx = MockNativeContext::new();
        let obj = ctx.alloc_object(ClassId::new(0), 4);
        // Unset slots default to Int(0).
        assert_eq!(ctx.get_field(obj, 2), Value::Int(0));
        ctx.set_field(obj, 2, Value::Long(0x1234_5678_9abc_def0));
        assert_eq!(ctx.get_field(obj, 2), Value::Long(0x1234_5678_9abc_def0));
        // Slot 2 round-trip must not disturb slot 1.
        assert_eq!(ctx.get_field(obj, 1), Value::Int(0));
    }

    #[test]
    fn identity_hash_is_stable_per_object_and_distinct_across_objects() {
        let ctx = MockNativeContext::new();
        let a = ctx.fresh_object_ref();
        let b = ctx.fresh_object_ref();
        let ha1 = ctx.identity_hash_code(a);
        let ha2 = ctx.identity_hash_code(a);
        let hb = ctx.identity_hash_code(b);
        // Same object → same hash on repeated lookup.
        assert_eq!(ha1, ha2, "identity hash must be stable across calls");
        // Distinct objects → distinct hashes (counter is monotonic).
        assert_ne!(ha1, hb, "distinct objects must hash differently");
    }

    #[test]
    fn invoke_virtual_returns_scripted_result_then_default() {
        let mut ctx = MockNativeContext::new();
        let recv = ctx.fresh_object_ref();
        ctx.set_invoke_virtual_result(Ok(Some(Value::Int(42))));

        // First call drains the scripted result.
        let first = ctx
            .invoke_virtual(recv, "magicAnswer", "()I", &[])
            .expect("scripted Ok result");
        assert_eq!(first, Some(Value::Int(42)));

        // Second call falls back to Ok(None) (Java void).
        let second = ctx
            .invoke_virtual(recv, "magicAnswer", "()I", &[])
            .expect("default Ok(None) on unscripted call");
        assert_eq!(second, None);
    }

    #[test]
    fn default_atomic_fetch_add_int_drives_cas_loop_against_mock() {
        // The default `atomic_fetch_add_int` impl in `NativeContext` is a
        // CAS retry loop over `get_field_volatile` + `compare_and_swap_field`.
        // The mock's HashMap-backed field store is exactly what the loop
        // needs to make progress, so this exercises the trait default
        // through the mock rather than a re-implementation.
        let mut ctx = MockNativeContext::new();
        let obj = ctx.alloc_object(ClassId::new(0), 1);
        ctx.set_field(obj, 0, Value::Int(10));

        let prev = ctx
            .atomic_fetch_add_int(obj, 0, 5)
            .expect("CAS loop should succeed against a fresh Int field");
        assert_eq!(prev, 10, "fetch_add should return the previous value");
        assert_eq!(ctx.get_field(obj, 0), Value::Int(15));
    }

    fn fill_int_array(ctx: &MockNativeContext, arr: ObjectRef, values: &[i32]) {
        for (i, value) in values.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*value));
        }
    }

    fn read_int_array(ctx: &MockNativeContext, arr: ObjectRef) -> Vec<i32> {
        (0..ctx.array_length(arr))
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(value) => value,
                other => panic!("expected Int array element, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn default_native_memory_copy_fails_closed() {
        let mut ctx = MockNativeContext::new();
        let src = [1u8, 2, 3, 4];
        let mut out = [0xAAu8; 4];

        assert!(!ctx.copy_from_native_memory(src.as_ptr() as i64, &mut out));
        assert_eq!(out, [0xAA; 4], "failed read must not mutate output");

        let mut dst = [0u8; 4];
        assert!(!ctx.copy_to_native_memory(dst.as_mut_ptr() as i64, &[9, 8, 7, 6]));
        assert_eq!(dst, [0; 4], "failed write must not touch destination");
    }

    #[test]
    fn default_bulk_array_copy_copies_matching_primitive_arrays() {
        let mut ctx = MockNativeContext::new();
        let src = ctx.new_array(ArrayElementType::Int, 4);
        let dst = ctx.new_array(ArrayElementType::Int, 4);
        fill_int_array(&ctx, src, &[10, 20, 30, 40]);

        assert!(ctx.bulk_array_copy(src, 1, dst, 0, 3));
        assert_eq!(read_int_array(&ctx, dst), vec![20, 30, 40, 0]);
    }

    #[test]
    fn default_bulk_array_copy_handles_same_array_overlap() {
        let mut ctx = MockNativeContext::new();
        let arr = ctx.new_array(ArrayElementType::Int, 5);
        fill_int_array(&ctx, arr, &[1, 2, 3, 4, 5]);

        assert!(ctx.bulk_array_copy(arr, 0, arr, 1, 4));
        assert_eq!(read_int_array(&ctx, arr), vec![1, 1, 2, 3, 4]);
    }

    #[test]
    fn default_bulk_array_copy_rejects_length_and_offset_overflow() {
        let mut ctx = MockNativeContext::new();
        let src = ctx.new_array(ArrayElementType::Int, 3);
        let dst = ctx.new_array(ArrayElementType::Int, 2);
        fill_int_array(&ctx, src, &[1, 2, 3]);
        fill_int_array(&ctx, dst, &[9, 9]);

        assert!(!ctx.bulk_array_copy(src, 0, dst, 0, 3));
        assert_eq!(read_int_array(&ctx, dst), vec![9, 9]);

        assert!(!ctx.bulk_array_copy(src, usize::MAX, dst, 0, 1));
        assert_eq!(read_int_array(&ctx, dst), vec![9, 9]);
    }

    #[test]
    fn default_bulk_array_copy_validates_zero_length_operands() {
        let mut ctx = MockNativeContext::new();
        let src = ctx.new_array(ArrayElementType::Int, 3);
        let dst = ctx.new_array(ArrayElementType::Int, 3);
        fill_int_array(&ctx, dst, &[7, 8, 9]);

        assert!(ctx.bulk_array_copy(src, 3, dst, 3, 0));
        assert_eq!(read_int_array(&ctx, dst), vec![7, 8, 9]);

        assert!(!ctx.bulk_array_copy(src, 4, dst, 0, 0));
        assert!(!ctx.bulk_array_copy(src, 0, dst, 4, 0));

        let non_array = ctx.alloc_object(ClassId::new(0), 0);
        assert!(!ctx.bulk_array_copy(non_array, 0, dst, 0, 0));

        let bytes = ctx.new_array(ArrayElementType::Byte, 1);
        assert!(!ctx.bulk_array_copy(bytes, 0, dst, 0, 0));

        let refs = ctx.new_ref_array(ClassId::new(0), 1);
        assert!(!ctx.bulk_array_copy(refs, 0, refs, 0, 0));
        assert_eq!(read_int_array(&ctx, dst), vec![7, 8, 9]);
    }

    #[test]
    fn default_bulk_array_copy_rejects_type_mismatch_and_references() {
        let mut ctx = MockNativeContext::new();
        let bytes = ctx.new_array(ArrayElementType::Byte, 2);
        let ints = ctx.new_array(ArrayElementType::Int, 2);
        fill_int_array(&ctx, bytes, &[1, 2]);
        fill_int_array(&ctx, ints, &[7, 7]);

        assert!(!ctx.bulk_array_copy(bytes, 0, ints, 0, 2));
        assert_eq!(read_int_array(&ctx, ints), vec![7, 7]);

        let refs = ctx.new_ref_array(ClassId::new(0), 1);
        assert!(!ctx.bulk_array_copy(refs, 0, refs, 0, 1));
    }
}
