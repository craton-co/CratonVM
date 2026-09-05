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
use std::collections::{HashMap, HashSet};

use cratonvm_native_api::{
    AnnotationData, AnnotationElementValue, FieldMetadata, MethodMetadata, NativeClassAccess,
    NativeContext, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess, StackTraceEntry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

/// A recorded `invoke_virtual` call.
#[derive(Debug, Clone)]
pub(crate) struct InvokeCall {
    pub declared_class: Option<String>,
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

/// The mock heap's own object-kind discriminant.
///
/// W7-83: this enum has always known which entries are arrays, and until
/// 2026-08-12 nothing that answered a *kind* question consulted it —
/// `heap_kind_of` returned `ObjectKind::Object` unconditionally,
/// `heap_element_type_of` returned `ArrayElementType::Reference`
/// unconditionally, and `object_is_array` was left on the trait default
/// `false`. Every unit test in this crate runs against this mock, so those
/// three constants made an entire class of screens untestable: a native that
/// asks "is this actually an array?" gets the same answer for a `byte[]` and
/// for a `MemorySegment`, and any test of the screen passes whether or not the
/// screen is correct. See W7-83-segment-as-backing-array.md §2.
///
/// `Array` therefore carries its `element_type` now: without it
/// `heap_element_type_of` cannot answer honestly even after it starts
/// consulting the discriminant, and `new_array`'s element-type argument was
/// being dropped on the floor.
enum HeapEntry {
    Object {
        fields: Vec<Value>,
    },
    Array {
        element_type: ArrayElementType,
        elements: Vec<Value>,
    },
}

/// The value a freshly allocated array of `element_type` reads back as, which
/// is NOT `Int(0)` for every kind: a reference array reads `Object(None)`, and
/// a `long`/`float`/`double` array reads the correspondingly typed zero. The
/// mock used to fill every array with `Value::Int(0)`, so a native that
/// distinguishes "unset reference slot" from "integer zero" could not be
/// tested here at all. Same table as `native-api/src/test_mock.rs`'s
/// `default_array_value`, which is the mock that already got this right.
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

pub(crate) struct MockNativeContext {
    heap: UnsafeCell<Vec<HeapEntry>>,
    ptr_to_index: UnsafeCell<HashMap<usize, usize>>,
    named_fields: UnsafeCell<HashMap<(usize, String), Value>>,
    field_reads: UnsafeCell<Vec<(usize, usize)>>,
    next_ptr: usize,
    pub scripts: Vec<InvokeScript>,
    pub calls: UnsafeCell<Vec<InvokeCall>>,
    blocking_begin_count: usize,
    blocking_end_count: usize,
    /// Optional `ObjectRef` -> Rust-side `String` mapping so `read_string`
    /// can return a real value. Populated lazily by `attach_string` /
    /// `create_string` so most tests pay nothing for it.
    pub strings: UnsafeCell<HashMap<usize, String>>,
    /// Optional per-object class identity. `class_table[0]` is the sentinel
    /// "unknown" name so `ClassId::new(0)` maps back to `None`. Objects are
    /// only entered here via `alloc_object_with_class`; everything else stays
    /// unknown, preserving the historic `class_id_of_object == 0` default.
    class_table: Vec<String>,
    obj_class: HashMap<usize, ClassId>,
    declared_methods: HashSet<(ClassId, String, String)>,
    /// Declared field slots, keyed by `(ClassId, field name)`.
    ///
    /// EMPTY BY DEFAULT, which reproduces the historic behaviour exactly:
    /// `resolve_field_index_by_class_id` answers `None` for every class no
    /// test has described. That matters because the stub it replaces returned
    /// `None` unconditionally, and a native whose fast path is gated on a
    /// resolvable layout would therefore REFUSE in every unit test — passing
    /// while proving nothing, because it was testing the mock rather than the
    /// native. See `declare_field`.
    field_slots: HashMap<(ClassId, String), usize>,
    /// Unique per mock instance — see the `vm_identity` impl.
    vm_identity: usize,
    /// Scripted `InputStream.read([BII)I` payload. `invoke_virtual` serves
    /// bytes from the front of this queue, honouring the caller's requested
    /// length, and returns `-1` once it is empty — i.e. it behaves like a real
    /// stream rather than a fixed scripted return value, which is what
    /// `stream_decoder`'s refill loop needs to be exercised end to end.
    stream_bytes: UnsafeCell<std::collections::VecDeque<u8>>,
    /// Set once `stream_bytes` has been supplied, so the mock knows to serve
    /// `read([BII)I` itself instead of falling through to `scripts`.
    stream_scripted: bool,
    /// Global roots handed out by `add_global_root`.
    global_roots: HashMap<usize, ObjectRef>,
    next_gref: usize,
    /// When set, indexed slots 0..=4 ALIAS the real-JDK `java.nio.Buffer`
    /// fields, exactly as they do on a loaded `Buffer` subclass:
    /// `mark(0) position(1) limit(2) capacity(3) address(4)`.
    ///
    /// The mock normally keeps indexed and by-name fields in two independent
    /// maps, which is the one thing that makes the nio `address` defect
    /// invisible to a test: the bug IS that a synthetic indexed write lands on
    /// a real by-name field. Without this, a test for it passes whether or not
    /// the fix is present. Off by default — every other test relies on the two
    /// maps staying independent.
    buffer_field_aliasing: bool,
}

/// The real-JDK `java.nio.Buffer` field order, by declaration index.
pub(crate) const BUFFER_ALIASED_FIELDS: [&str; 5] =
    ["mark", "position", "limit", "capacity", "address"];

impl MockNativeContext {
    pub(crate) fn new() -> Self {
        Self {
            heap: UnsafeCell::new(Vec::new()),
            ptr_to_index: UnsafeCell::new(HashMap::new()),
            named_fields: UnsafeCell::new(HashMap::new()),
            field_reads: UnsafeCell::new(Vec::new()),
            next_ptr: 8,
            scripts: Vec::new(),
            calls: UnsafeCell::new(Vec::new()),
            blocking_begin_count: 0,
            blocking_end_count: 0,
            strings: UnsafeCell::new(HashMap::new()),
            class_table: vec![String::new()],
            obj_class: HashMap::new(),
            declared_methods: HashSet::new(),
            field_slots: HashMap::new(),
            vm_identity: {
                static NEXT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(1);
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            },
            stream_bytes: UnsafeCell::new(std::collections::VecDeque::new()),
            stream_scripted: false,
            global_roots: HashMap::new(),
            next_gref: 1,
            buffer_field_aliasing: false,
        }
    }

    /// Model a real-JDK `java.nio.Buffer` layout: indexed slots 0..=4 and the
    /// by-name fields `mark`/`position`/`limit`/`capacity`/`address` become the
    /// same storage. See [`MockNativeContext::buffer_field_aliasing`].
    pub(crate) fn alias_nio_buffer_fields(&mut self) {
        self.buffer_field_aliasing = true;
    }

    /// Make `invoke_virtual(_, "read", "([BII)I", ...)` behave like a real
    /// `InputStream` over `bytes`: it fills the caller's array with up to the
    /// requested length and returns the count, then `-1` at exhaustion.
    pub(crate) fn script_input_stream(&mut self, bytes: &[u8]) {
        // SAFETY: the mock is single-threaded and `&mut self` excludes every
        // other access to this UnsafeCell for the duration of the borrow.
        let q = unsafe { &mut *self.stream_bytes.get() };
        q.clear();
        q.extend(bytes.iter().copied());
        self.stream_scripted = true;
    }

    /// Serve one `read([BII)I` from the scripted stream.
    fn serve_stream_read(&mut self, args: &[Value]) -> MethodCallResult {
        let arr = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let off = match args.get(1) {
            Some(Value::Int(v)) => *v as usize,
            _ => 0,
        };
        let want = match args.get(2) {
            Some(Value::Int(v)) => *v as usize,
            _ => 0,
        };
        let chunk: Vec<u8> = {
            // SAFETY: the mock is single-threaded and `&mut self` excludes
            // concurrent or aliased access to the scripted queue.
            let q = unsafe { &mut *self.stream_bytes.get() };
            let n = want.min(q.len());
            q.drain(..n).collect()
        };
        if chunk.is_empty() {
            return Ok(Some(Value::Int(-1)));
        }
        for (i, b) in chunk.iter().enumerate() {
            self.set_array_element(arr, off + i, Value::Int(*b as i8 as i32));
        }
        Ok(Some(Value::Int(chunk.len() as i32)))
    }

    fn ensure_mock_class(&mut self, class_name: &str) -> ClassId {
        match self.class_table.iter().position(|n| n == class_name) {
            Some(i) => ClassId::new(i as u32),
            None => {
                self.class_table.push(class_name.to_string());
                ClassId::new((self.class_table.len() - 1) as u32)
            }
        }
    }

    /// Allocate an object whose `class_id_of_object` / `class_name_of_id`
    /// resolve to `class_name`. Used by tests that exercise natives which
    /// branch on the receiver's runtime class (e.g. the `ByteArrayInputStream`
    /// fast path in `read([BII)`).
    pub(crate) fn alloc_object_with_class(
        &mut self,
        num_fields: usize,
        class_name: &str,
    ) -> ObjectRef {
        let obj = self.alloc_object(num_fields);
        let cid = self.ensure_mock_class(class_name);
        self.obj_class.insert(obj.as_ptr() as usize, cid);
        obj
    }

    /// Give `class_name` a field at slot `index`, so
    /// `resolve_field_index_by_class_id` can answer for it.
    ///
    /// Tests that exercise a native which memoizes or resolves field slots
    /// MUST call this, and should also assert the negative case (an
    /// undescribed class still refuses) — otherwise a passing test cannot
    /// distinguish "the fast path ran and was correct" from "the fast path
    /// refused and the slow path was correct", which are the two outcomes a
    /// slot-resolution stub silently merges.
    pub(crate) fn declare_field(&mut self, class_name: &str, field: &str, index: usize) {
        let cid = self.ensure_mock_class(class_name);
        self.field_slots.insert((cid, field.to_string()), index);
    }

    pub(crate) fn declare_method(&mut self, class_name: &str, method: &str, desc: &str) {
        let cid = self.ensure_mock_class(class_name);
        self.declared_methods
            .insert((cid, method.to_string(), desc.to_string()));
    }

    fn strings_mut(&self) -> &mut HashMap<usize, String> {
        // SAFETY: this test-only context is never shared between threads and
        // callers do not retain a second reference across another mock call.
        unsafe { &mut *self.strings.get() }
    }
    fn strings_ref(&self) -> &HashMap<usize, String> {
        // SAFETY: same single-threaded mock invariant as `strings_mut`; no
        // mutable borrow is live while this shared reference is used.
        unsafe { &*self.strings.get() }
    }

    /// Allocate a dummy object and associate it with the given string so
    /// `ctx.read_string(obj)` returns `Some(text)`. Used by tests that
    /// exercise natives reading guest-supplied path/string arguments.
    pub(crate) fn attach_string(&mut self, text: &str) -> ObjectRef {
        let obj = self.alloc_object(0);
        let ptr = obj.as_ptr() as usize;
        self.strings_mut().insert(ptr, text.to_string());
        obj
    }

    fn heap_mut(&self) -> &mut Vec<HeapEntry> {
        // SAFETY: test-only single-threaded mock; helper borrows are scoped to
        // one call and never overlap.
        unsafe { &mut *self.heap.get() }
    }
    fn heap_ref(&self) -> &Vec<HeapEntry> {
        // SAFETY: no mutable heap helper borrow is live at this call site.
        unsafe { &*self.heap.get() }
    }
    fn ptr_map_mut(&self) -> &mut HashMap<usize, usize> {
        // SAFETY: test-only single-threaded mock; helper borrows never overlap.
        unsafe { &mut *self.ptr_to_index.get() }
    }
    fn ptr_map_ref(&self) -> &HashMap<usize, usize> {
        // SAFETY: no mutable pointer-map helper borrow is live here.
        unsafe { &*self.ptr_to_index.get() }
    }
    fn named_fields_mut(&self) -> &mut HashMap<(usize, String), Value> {
        // SAFETY: test-only single-threaded mock; helper borrows never overlap.
        unsafe { &mut *self.named_fields.get() }
    }
    fn named_fields_ref(&self) -> &HashMap<(usize, String), Value> {
        // SAFETY: no mutable named-field helper borrow is live here.
        unsafe { &*self.named_fields.get() }
    }

    fn alloc_entry(&mut self, entry: HeapEntry) -> ObjectRef {
        let idx = self.heap_mut().len();
        self.heap_mut().push(entry);
        let ptr = self.next_ptr;
        self.next_ptr += 8;
        self.ptr_map_mut().insert(ptr, idx);
        // SAFETY: the mock assigns unique, non-null, aligned sentinel
        // addresses and records each one in ptr_to_index before exposure.
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

    /// Number of global roots currently held — a non-zero value after a
    /// native has fully completed is a root leak.
    pub(crate) fn global_root_count(&self) -> usize {
        self.global_roots.len()
    }

    /// Model a moving collection: repoint an existing global root at the
    /// object's post-copy address, exactly as the collector's remap pass does
    /// to the JNI/global-root table.
    ///
    /// This is what makes "is it remapped?" testable. Code that resolves
    /// through the handle sees the NEW address; code that kept a bare
    /// `ObjectRef` from before the move still sees the old one — which is the
    /// defect, and the assertion that separates the two.
    pub(crate) fn relocate_global_root(&mut self, handle: usize, new_obj: ObjectRef) {
        assert!(
            self.global_roots.contains_key(&handle),
            "relocate_global_root: handle {handle} is not a live root"
        );
        self.global_roots.insert(handle, new_obj);
    }

    pub(crate) fn blocking_region_counts(&self) -> (usize, usize) {
        (self.blocking_begin_count, self.blocking_end_count)
    }

    pub(crate) fn recorded_calls(&self) -> &[InvokeCall] {
        // SAFETY: the test-only mock is single-threaded and no writer borrow is
        // alive while a test observes this returned slice.
        unsafe { &*self.calls.get() }
    }

    pub(crate) fn field_read_count(&self, obj: ObjectRef, index: usize) -> usize {
        let ptr = obj.as_ptr() as usize;
        // SAFETY: the mock is single-threaded and no mutable field-read borrow
        // overlaps this read-only inspection.
        unsafe { &*self.field_reads.get() }
            .iter()
            .filter(|(read_obj, read_index)| *read_obj == ptr && *read_index == index)
            .count()
    }
}

impl cratonvm_native_api::NativeClassAccess for MockNativeContext {
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

    // --- everything else: default stubs (unused by the test) ---
    fn load_class(&mut self, _n: &str) -> MethodCallResult {
        Ok(None)
    }
    fn class_name_of_id(&self, c: ClassId) -> Option<String> {
        match self.class_table.get(c.as_u32() as usize) {
            Some(name) if !name.is_empty() => Some(name.clone()),
            _ => None,
        }
    }
    fn class_id_of_object(&self, o: ObjectRef) -> ClassId {
        self.obj_class
            .get(&(o.as_ptr() as usize))
            .copied()
            .unwrap_or_else(|| ClassId::new(0))
    }
    fn method_exists(&self, _c: &str, _m: &str, _d: &str) -> bool {
        false
    }
    fn class_declares_method(&self, class_id: ClassId, name: &str, descriptor: &str) -> bool {
        self.declared_methods
            .contains(&(class_id, name.to_string(), descriptor.to_string()))
    }
    fn ensure_class_initialized(&mut self, n: &str) -> Result<ClassId, MethodCallFailed> {
        Ok(self.ensure_mock_class(n))
    }
    fn is_subclass(&self, _c: ClassId, _p: ClassId) -> bool {
        false
    }
    fn superclass_of(&self, _c: ClassId) -> Option<ClassId> {
        None
    }
    fn class_id_by_name(&self, n: &str) -> Option<ClassId> {
        self.class_table
            .iter()
            .position(|name| name == n)
            .map(|i| ClassId::new(i as u32))
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
        self.alloc_object(0)
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
}

impl cratonvm_native_api::NativeInvokeAccess for MockNativeContext {
    fn invoke_virtual(
        &mut self,
        _receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // Record the call so tests can assert on it.
        // SAFETY: NativeContext invocation takes `&mut self`; no other call can
        // access the test-only call log concurrently or retain its borrow.
        let calls = unsafe { &mut *self.calls.get() };
        calls.push(InvokeCall {
            declared_class: None,
            method_name: method_name.to_string(),
            descriptor: descriptor.to_string(),
            args: args.to_vec(),
        });
        // Scripted InputStream takes precedence over the static script table.
        if self.stream_scripted && method_name == "read" && descriptor == "([BII)I" {
            return self.serve_stream_read(args);
        }
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

    fn invoke_virtual_declared(
        &mut self,
        declared_class: &str,
        _receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // SAFETY: as in `invoke_virtual`, `&mut self` provides exclusive
        // access to the test-only call log.
        let calls = unsafe { &mut *self.calls.get() };
        calls.push(InvokeCall {
            declared_class: Some(declared_class.to_string()),
            method_name: method_name.to_string(),
            descriptor: descriptor.to_string(),
            args: args.to_vec(),
        });
        if let Some(pos) = self
            .scripts
            .iter()
            .position(|s| s.method_name == method_name && s.descriptor == descriptor)
        {
            return self.scripts.remove(pos).result;
        }
        Ok(None)
    }
    fn invoke(&mut self, c: &str, m: &str, d: &str, a: &[Value]) -> MethodCallResult {
        // Record interface-dispatch calls too. `drain_completions` delivers
        // `CompletionHandler.completed`/`failed` through this entry point, and
        // the root-audit tests need to see WHICH handler reference it passed —
        // the whole point of resolving through the global root is that the
        // address changes across a relocation.
        // SAFETY: `&mut self` gives exclusive access to the test-only log.
        let calls = unsafe { &mut *self.calls.get() };
        calls.push(InvokeCall {
            declared_class: Some(c.to_string()),
            method_name: m.to_string(),
            descriptor: d.to_string(),
            args: a.to_vec(),
        });
        Ok(None)
    }
}

impl cratonvm_native_api::NativeHeapAccess for MockNativeContext {
    // --- minimal heap primitives used by the native under test ---
    fn new_array(&mut self, et: ArrayElementType, length: usize) -> ObjectRef {
        // W7-83: `et` used to be `_et`. The mock allocated every array as a
        // vector of `Int(0)` and then answered `heap_element_type_of` with a
        // constant, so the element type a test asked for was unobservable.
        self.alloc_entry(HeapEntry::Array {
            element_type: et,
            elements: vec![default_array_value(et); length],
        })
    }
    fn array_length(&self, obj: ObjectRef) -> usize {
        match &self.heap_ref()[self.entry_index(obj)] {
            HeapEntry::Array { elements, .. } => elements.len(),
            _ => 0,
        }
    }
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
        match &self.heap_ref()[self.entry_index(obj)] {
            HeapEntry::Array {
                elements,
                element_type,
            } => elements
                .get(index)
                .copied()
                .unwrap_or_else(|| default_array_value(*element_type)),
            // Not an array. The production contract is that the caller has
            // already screened the receiver's kind (`heap_kind_of`), so this
            // arm is reached only by code that did not — it is kept as a
            // fail-safe rather than a panic, but see
            // `heap_kind_of`/`object_is_array`, which are what a caller is now
            // able to ask FIRST.
            _ => Value::Int(0),
        }
    }
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
        let idx = self.entry_index(obj);
        if let HeapEntry::Array { elements, .. } = &mut self.heap_mut()[idx] {
            if index < elements.len() {
                elements[index] = value;
            }
        }
    }
    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        // SAFETY: the test context is single-threaded; this short mutation of
        // the instrumentation log cannot overlap another borrow.
        unsafe { &mut *self.field_reads.get() }.push((obj.as_ptr() as usize, index));
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
        if self.buffer_field_aliasing {
            if let Some(name) = BUFFER_ALIASED_FIELDS.get(index) {
                self.named_fields_mut()
                    .insert((obj.as_ptr() as usize, (*name).to_string()), value);
            }
        }
    }
    /// Allocate an object of `class`, tagged so `class_id_of_object` /
    /// `class_name_of_id` identify it.
    ///
    /// This used to return `Ok(None)`, which is not "no opinion" — it is
    /// "allocation failed", and natives have real fallback branches for that.
    /// `async_socket::drain_completions` takes one: if it cannot build the
    /// `java.io.IOException` for a failed op it drops the completion and
    /// delivers nothing. So `audit_failed_delivery_is_remapped_and_releases_roots`
    /// could never dispatch `failed()`, and was red from the day it landed —
    /// the production path was correct, the mock could not express it.
    fn new_object(&mut self, class: &str) -> MethodCallResult {
        // Zero declared fields: this mock keeps instance state in the
        // name-keyed side map (`set_field_by_name`), which does not consult the
        // field vector, and `detailMessage` is written that way.
        let obj = self.alloc_object_with_class(0, class);
        Ok(Some(Value::Object(Some(obj))))
    }

    /// Allocate and "construct". The trait default runs `<init>` through
    /// [`Self::invoke`], which in this mock only RECORDS the call — so a
    /// `Throwable(String)` would come back with no message, and a native that
    /// prefers the real constructor over a synthetic fallback (e.g.
    /// `afc_io_exception`) would deliver an exception whose `detailMessage` is
    /// null. Emulate the one constructor shape that matters here:
    /// `(Ljava/lang/String;)V` stores its argument in `detailMessage`, exactly
    /// as every `Throwable(String)` does.
    fn new_object_initialized(
        &mut self,
        class_name: &str,
        init_desc: &str,
        init_args: &[Value],
    ) -> MethodCallResult {
        let obj_val = self.new_object(class_name)?;
        if let Some(Value::Object(Some(obj))) = obj_val {
            let mut full = Vec::with_capacity(init_args.len() + 1);
            full.push(Value::Object(Some(obj)));
            full.extend_from_slice(init_args);
            self.invoke(class_name, "<init>", init_desc, &full)?;
            if init_desc == "(Ljava/lang/String;)V" {
                if let Some(message @ Value::Object(Some(_))) = init_args.first() {
                    self.set_field_by_name(obj, "detailMessage", *message);
                }
            }
        }
        Ok(obj_val)
    }
    fn identity_hash_code(&self, o: ObjectRef) -> i32 {
        o.as_ptr() as i32
    }
    fn get_field_by_name(&self, o: ObjectRef, n: &str) -> Value {
        self.named_fields_ref()
            .get(&(o.as_ptr() as usize, n.to_string()))
            .copied()
            .unwrap_or(Value::Object(None))
    }
    fn set_field_by_name(&self, o: ObjectRef, n: &str, v: Value) {
        self.named_fields_mut()
            .insert((o.as_ptr() as usize, n.to_string()), v);
    }
    fn resolve_field_index(&self, class_name: &str, field: &str) -> Option<usize> {
        let cid = self.class_table.iter().position(|n| n == class_name)?;
        self.field_slots
            .get(&(ClassId::new(cid as u32), field.to_string()))
            .copied()
    }
    // Was an unconditional `None` stub. That is not a neutral default: it made
    // every fast path gated on a resolvable layout refuse inside every unit
    // test, so such a test asserted the SLOW path's answer and reported it as
    // the fast path's. Now backed by `declare_field`, and still `None` for any
    // class a test has not described — so no existing test changes behaviour.
    fn resolve_field_index_by_class_id(&self, class_id: ClassId, field: &str) -> Option<usize> {
        self.field_slots
            .get(&(class_id, field.to_string()))
            .copied()
    }
    fn new_ref_array(&mut self, _c: ClassId, length: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Array {
            element_type: ArrayElementType::Reference,
            elements: vec![Value::Object(None); length],
        })
    }
    /// W7-83. Was `ObjectKind::Object`, unconditionally, for every object on
    /// this mock's heap — including the ones `new_array`/`new_ref_array` had
    /// just allocated as `HeapEntry::Array`. The discriminant was right there
    /// and nothing consulted it.
    ///
    /// This is not a cosmetic repair. `bb_resolve_heap_array` needs to reject a
    /// `java.nio.Buffer.segment` that is a `MemorySegment` rather than a
    /// `byte[]`, and the only honest screen is a kind question. Against the old
    /// constant that screen rejected every array too, so
    /// `bb_get_bulk_reads_real_heap_layout_slot_hb` — a test that legitimately
    /// stashes a real array at slot 5 — would have gone red for a reason that
    /// has nothing to do with the screen. Fixing the mock first is what makes
    /// the screen's test able to fail for the right reason.
    fn heap_kind_of(&self, o: ObjectRef) -> ObjectKind {
        match &self.heap_ref()[self.entry_index(o)] {
            HeapEntry::Array { .. } => ObjectKind::Array,
            HeapEntry::Object { .. } => ObjectKind::Object,
        }
    }
    /// W7-83. Was `ArrayElementType::Reference`, unconditionally. The trait
    /// specifies `Reference` for a NON-array and for a reference array, so the
    /// non-array arm below is the contract, not a fallback; the array arm now
    /// answers what the array was actually allocated as.
    fn heap_element_type_of(&self, o: ObjectRef) -> ArrayElementType {
        match &self.heap_ref()[self.entry_index(o)] {
            HeapEntry::Array { element_type, .. } => *element_type,
            HeapEntry::Object { .. } => ArrayElementType::Reference,
        }
    }
    /// W7-83. Was the trait default `false` — i.e. "this context has no heap",
    /// which is exactly wrong for a mock that does. The trait's own doc says
    /// the default is for "mock contexts without a heap"; this one has one and
    /// must answer from it, or a native's array/instance fork is untestable
    /// here in either direction.
    fn object_is_array(&self, o: ObjectRef) -> bool {
        matches!(
            &self.heap_ref()[self.entry_index(o)],
            HeapEntry::Array { .. }
        )
    }
    fn create_string(&mut self, t: &str) -> ObjectRef {
        let obj = self.alloc_object(0);
        let ptr = obj.as_ptr() as usize;
        self.strings_mut().insert(ptr, t.to_string());
        obj
    }
    fn read_string(&self, o: ObjectRef) -> Option<String> {
        let ptr = o.as_ptr() as usize;
        self.strings_ref().get(&ptr).cloned()
    }
    fn get_class_mirror(&mut self, _c: ClassId) -> ObjectRef {
        self.alloc_object(0)
    }
    fn alloc_object(&mut self, c: ClassId, num_fields: usize) -> ObjectRef {
        let obj = MockNativeContext::alloc_object(self, num_fields);
        self.obj_class.insert(obj.as_ptr() as usize, c);
        obj
    }
    /// W7-83 re-checked this one and left it: an array HAS no instance fields,
    /// so `0` is the honest answer for the `Array` arm rather than a stub. Note
    /// what it is NOT, though — it is not a width witness. `new_object` here
    /// allocates with zero declared fields and keeps its state in the
    /// name-keyed side map, so `object_num_fields` answers 0 for most objects
    /// this mock hands out, and a production predicate of the
    /// `object_num_fields(buf) != 6` species (W7-76 §6) cannot be exercised
    /// against it without `alloc_object(n)`.
    fn object_num_fields(&self, obj: ObjectRef) -> usize {
        match &self.heap_ref()[self.entry_index(obj)] {
            HeapEntry::Object { fields } => fields.len(),
            HeapEntry::Array { .. } => 0,
        }
    }
    fn heap_allocated_bytes(&self) -> usize {
        0
    }
    fn get_field_volatile(&self, o: ObjectRef, i: usize) -> Value {
        self.get_field(o, i)
    }
    fn set_field_volatile(&self, o: ObjectRef, i: usize, v: Value) {
        self.set_field(o, i, v)
    }
    fn compare_and_swap_field(&mut self, _o: ObjectRef, _i: usize, _e: Value, _n: Value) -> bool {
        false
    }
    // Real global-root bookkeeping (the trait defaults are inert stubs that
    // hand back handle 0), so tests can exercise natives that park a Java
    // object for a worker thread and resolve it again on delivery.
    fn add_global_root(&mut self, obj: ObjectRef) -> usize {
        let h = self.next_gref;
        self.next_gref += 1;
        self.global_roots.insert(h, obj);
        h
    }
    fn resolve_global_root(&self, handle: usize) -> Option<ObjectRef> {
        self.global_roots.get(&handle).copied()
    }
    fn remove_global_root(&mut self, handle: usize) -> bool {
        self.global_roots.remove(&handle).is_some()
    }
    fn allocate_instance(&mut self, _c: &str) -> Option<ObjectRef> {
        None
    }
    fn discover_reference(&mut self, _t: u8, _r: ObjectRef, _f: ObjectRef, _q: Option<ObjectRef>) {}
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
        self.alloc_object(0)
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
    fn begin_blocking_region(&mut self) {
        self.blocking_begin_count += 1;
    }
    fn end_blocking_region(&mut self) {
        self.blocking_end_count += 1;
    }
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
    /// Every mock is its OWN VM.
    ///
    /// The trait default is `0` for every context, which makes any native-side
    /// cache scoped by `vm_identity` behave as though all tests shared one VM.
    /// Since each mock restarts its class table at `ClassId(1)`, unrelated
    /// tests then collide on that id and poison each other's cached layouts —
    /// which is not merely a test artefact but the multi-VM embedding case in
    /// miniature. Modelling it here is what makes such a cache's scoping
    /// testable at all; without it a cross-VM cache bug passes every test.
    fn vm_identity(&self) -> usize {
        self.vm_identity
    }

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
    fn try_ensure_synthetic_class(
        &mut self,
        name: &str,
        _num_fields: usize,
    ) -> Result<ClassId, cratonvm_native_api::ClassIdentityError> {
        // The mock has no compatibility policy, so it never refuses — it is
        // standing in for `Compatible` mode, where a fabrication always
        // succeeds. It overrode the infallible spelling until that was deleted
        // by JDK-only wave 2 step 3 (2026-08-10).
        Ok(self.ensure_mock_class(name))
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
        use std::sync::OnceLock;
        static FD: OnceLock<cratonvm_native_api::fd_table::FileDescriptorTable> = OnceLock::new();
        FD.get_or_init(cratonvm_native_api::fd_table::FileDescriptorTable::new)
    }
    fn allocate_native_memory(&mut self, _s: usize, _a: usize) -> Option<(i64, *mut u8)> {
        None
    }
    fn free_native_memory(&mut self, _a: i64) {}
    fn copy_from_native_memory(&self, addr: i64, out: &mut [u8]) -> bool {
        if out.is_empty() {
            return true;
        }
        if addr <= 0 {
            return false;
        }
        // SAFETY: this test helper is called only with a live native allocation
        // spanning `out.len()` bytes; slices guarantee a valid destination.
        unsafe {
            std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len());
        }
        true
    }
    fn copy_to_native_memory(&mut self, addr: i64, data: &[u8]) -> bool {
        if data.is_empty() {
            return true;
        }
        if addr <= 0 {
            return false;
        }
        // SAFETY: this test helper is called only with a live writable native
        // allocation spanning `data.len()` bytes; the source slice is valid.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len());
        }
        true
    }
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
}

// ---------------------------------------------------------------------------
// Cross-module test serialization for `set_path_confine_to_cwd`.
//
// `PATH_CONFINE_TO_CWD` is a global atomic; tests that flip it briefly
// would otherwise race with parallel tests that depend on it being off
// (e.g. the WatchService tests in `watch.rs` register absolute /tmp
// paths). A single process-wide mutex guarantees only one confinement
// test runs at a time, and the test always pairs `set(true)` with
// `set(false)` while still holding the guard.
// ---------------------------------------------------------------------------

use parking_lot::Mutex as PlMutex;
use std::sync::OnceLock as StdOnceLock;

pub(crate) fn confine_test_lock() -> &'static PlMutex<()> {
    static LOCK: StdOnceLock<PlMutex<()>> = StdOnceLock::new();
    LOCK.get_or_init(|| PlMutex::new(()))
}
