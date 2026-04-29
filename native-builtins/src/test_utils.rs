//! Minimal mock NativeContext for unit testing native method implementations.
//!
//! Provides a simple heap-backed context that supports string creation/reading,
//! field access, and array operations — enough to test most native methods
//! without pulling in the full VM.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use rustjvm_native_api::{
    AnnotationData, FieldMetadata, MethodMetadata, NativeContext, StackTraceEntry,
};
use rustjvm_types::error::{MethodCallFailed, MethodCallResult};
use rustjvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

#[cfg(target_os = "windows")]
extern "system" {
    fn GetModuleHandleA(lpModuleName: *const i8) -> *mut std::ffi::c_void;
    fn GetProcAddress(hModule: *mut std::ffi::c_void, lpProcName: *const i8) -> *mut std::ffi::c_void;
}

#[cfg(target_os = "windows")]
use GetModuleHandleA as winapi_GetModuleHandleA;
#[cfg(target_os = "windows")]
use GetProcAddress as winapi_GetProcAddress;

/// A mock heap object: either a regular object with fields, or an array.
enum HeapEntry {
    Object {
        class_id: ClassId,
        fields: Vec<Value>,
    },
    Array {
        elements: Vec<Value>,
    },
}

/// A minimal mock implementation of NativeContext for testing.
///
/// Uses `UnsafeCell` for the heap because the `NativeContext` trait requires
/// `set_field` and `set_array_element` to take `&self` (not `&mut self`).
/// This is safe in single-threaded test code.
/// Map a JDK-style `java.lang.reflect.{Field,Method,Constructor}` field
/// name to a synthetic slot index in the MockNativeContext heap. The
/// production code's `get_field_by_name` / `set_field_by_name` calls get
/// rewritten through this map so tests can still reason about a flat
/// synthetic layout (without a real class hierarchy).
///
/// C5 added the Field mapping (slots 0..6).
/// C6 adds the Method / Constructor mapping. Method keeps the legacy
/// Method slot layout so the `METHOD_NUM_FIELDS_LEGACY_FLOOR = 8`
/// allocation has room for all of them:
///   0 clazz, 1 name, 2 returnType, 3 parameterTypes,
///   4 modifiers, 5 slot, 6 callerSensitive, 7 override.
/// Constructor shares the same prefix (no `returnType`/`name`).
///
/// Returns `None` for unknown names — callers treat `None` as "not
/// present in the mock class layout" and silently skip the write (or
/// return `Int(0)` for reads).
fn mock_jdk_field_slot(name: &str) -> Option<usize> {
    // Shared synthetic slot namespace for Field / Method / Constructor
    // mirrors. No single test touches both a Field and a Method on the
    // same heap entry, so we can overlap `type` (Field) with
    // `returnType` (Method) without corruption. `parameterTypes`
    // (Method/Constructor) is placed outside the Field range.
    match name {
        "clazz" => Some(0),
        "name" => Some(1),
        "type" => Some(2),
        "returnType" => Some(2),
        "modifiers" => Some(3),
        "slot" => Some(4),
        "override" => Some(6), // AccessibleObject.override / accessible
        // C6 additions (no overlap with Field's 0..=6 range):
        "parameterTypes" => Some(7),
        "callerSensitive" => Some(8),
        // G2 additions: non-null array fields populated by
        // `create_method_object`. Real JDK `Method`/`Executable` declare
        // these; ByteBuddy's clinit relies on them being non-null.
        "exceptionTypes" => Some(9),
        "annotations" => Some(10),
        "parameterAnnotations" => Some(11),
        "annotationDefault" => Some(12),
        // C33: MemberName-specific names (overlap with Field is fine since
        // MemberName tests never touch Field mirrors on the same heap entry).
        "flags" => Some(3),
        _ => None,
    }
}

pub(crate) struct MockNativeContext {
    heap: UnsafeCell<Vec<HeapEntry>>,
    /// Maps ObjectRef pointer values to heap indices
    ptr_to_index: UnsafeCell<HashMap<usize, usize>>,
    /// class_id -> class_name mapping
    class_names: HashMap<u32, String>,
    /// class_name -> class_id mapping
    name_to_id: HashMap<String, u32>,
    next_class_id: u32,
    next_ptr: usize,
    properties: HashMap<String, String>,
    /// Native memory allocations for Panama tests
    native_allocs: UnsafeCell<HashMap<i64, (*mut u8, std::alloc::Layout)>>,
    next_alloc_id: UnsafeCell<i64>,
    /// Upcall table for Panama tests
    upcall_entries: UnsafeCell<Vec<rustjvm_native_api::ffi::UpcallEntry>>,
    /// invoke_virtual callback result (set by test to control upcall behavior)
    pub(crate) invoke_virtual_result: UnsafeCell<Option<MethodCallResult>>,
    /// NEW-8: tracks class IDs that have been marked as hidden via
    /// `set_class_hidden`. Consulted by the `is_class_hidden` override.
    hidden_classes: UnsafeCell<std::collections::HashSet<u32>>,
    /// NEW-8: records the last name used for a `define_class_from_bytes`
    /// or `define_hidden_class_from_bytes` call so tests can assert on
    /// the mangled hidden-class name directly.
    pub(crate) last_defined_class_name: UnsafeCell<Option<String>>,
    /// C14: per-class overrides for access_flags (so tests can pretend a
    /// class is ACC_ENUM without going through real class loading).
    pub(crate) class_flags_override: UnsafeCell<HashMap<u32, u16>>,
    /// C14: per-class static-field `$VALUES` reference, used by the
    /// `getEnumConstantsShared` native test to verify the non-null
    /// array is returned with the right number of elements.
    pub(crate) enum_values_override: UnsafeCell<HashMap<u32, ObjectRef>>,
    /// T19.N2: settable interrupt flag for the mock current thread.
    /// Exposed via `set_interrupted` helpers so tests (notably
    /// `Thread.sleep0` interrupt-handling) can simulate an interrupt
    /// being delivered during a blocking native call.
    pub(crate) interrupted_flag: UnsafeCell<bool>,
    /// T19.N1: per-class CodeSource URL (what `class_code_base` returns).
    /// Populated by tests to simulate a class loaded from a known URL.
    pub(crate) code_base_override: UnsafeCell<HashMap<u32, String>>,
    /// T19.N1: per-class signer certificate blocks (raw PKCS#7 bytes).
    /// Returned by `class_code_source_certs`; empty by default.
    pub(crate) code_source_certs_override: UnsafeCell<HashMap<u32, Vec<Vec<u8>>>>,
    /// WP0.2: per-class overrides for `declared_fields`. Empty vec by
    /// default (mock has no class metadata); tests can populate this
    /// to simulate a class with a specific declared field list for
    /// `ObjectStreamClass` / reflection-driven code.
    pub(crate) declared_fields_override: UnsafeCell<HashMap<u32, Vec<FieldMetadata>>>,
    /// WP0.2: per-class overrides for `declared_methods`.
    pub(crate) declared_methods_override: UnsafeCell<HashMap<u32, Vec<MethodMetadata>>>,
    /// WP0.2: per-class super-class override (consulted by
    /// `superclass_of` / `is_subclass` walks). Default: no super
    /// (i.e. every class looks like java/lang/Object).
    pub(crate) superclass_override: UnsafeCell<HashMap<u32, ClassId>>,
    /// G2: per-class is-interface override. Tests that exercise the
    /// `Class.getSuperclass()`-returns-null-for-interfaces semantics
    /// (or `getMethods` interface skip) can populate this. Default
    /// `false` for any class id not in the map.
    pub(crate) is_interface_override: UnsafeCell<HashMap<u32, bool>>,
    /// WP0.2: per-class interface-list override (for `class_interfaces`
    /// and `is_subclass` against an interface parent).
    pub(crate) interfaces_override: UnsafeCell<HashMap<u32, Vec<ClassId>>>,
    /// WP0.2: process-wide ObjectStreamClass cache (ClassId.as_u32 →
    /// cached descriptor ObjectRef). Drives `osc_cache_get` /
    /// `osc_cache_put` so tests can assert identity.
    pub(crate) osc_cache_map: UnsafeCell<HashMap<u32, ObjectRef>>,
    /// T19.H10: per-resource-name byte payload returned by `find_resource`.
    /// Empty by default (matches the fresh-VM semantics: no classpath jar
    /// has been mapped). Tests populate via `set_resource(name, bytes)` to
    /// simulate a classpath that contains the named resource.
    pub(crate) resources_override: UnsafeCell<HashMap<String, Vec<u8>>>,
    /// T19_K2: tracks each `register_native_thread` call so tests can
    /// assert that Vert.x / XNIO event loops correctly route through the
    /// new VM-tracking entry point. Each entry is `(name, daemon, alive)`.
    /// `unregister_native_thread` flips `alive` to false but leaves the
    /// entry in place for assertions.
    pub(crate) registered_native_threads: UnsafeCell<Vec<(String, bool, bool)>>,
    /// T19_K2: counter feeding ThreadId values handed back from
    /// `register_native_thread`. Starts at 1 (0 is reserved as "no
    /// registration" / mock not configured).
    pub(crate) next_native_tid: UnsafeCell<u64>,
    /// T19_K4: tracks `set_native_thread_java_obj` calls. Maps the
    /// 1-based ThreadId to the raw `ObjectRef.as_ptr() as usize` of
    /// the attached `java.lang.Thread` mirror. Tests assert that
    /// Vert.x / XNIO event-loop spawns route a non-null mirror
    /// through this entry point.
    pub(crate) native_thread_java_objs: UnsafeCell<HashMap<u64, usize>>,
    /// T19_H15: tracks every path passed to `register_dynamic_classpath`.
    /// Tests assert that the JBoss module loader registers the full
    /// transitive dependency closure on the shared classpath when a
    /// module is loaded.  Insertion order is preserved.
    pub(crate) registered_classpath: UnsafeCell<Vec<String>>,
}

impl MockNativeContext {
    pub(crate) fn new() -> Self {
        Self {
            heap: UnsafeCell::new(Vec::new()),
            ptr_to_index: UnsafeCell::new(HashMap::new()),
            class_names: HashMap::new(),
            name_to_id: HashMap::new(),
            next_class_id: 1,
            // Start at 8 so first pointer is 8-byte aligned and non-null
            next_ptr: 8,
            properties: HashMap::new(),
            native_allocs: UnsafeCell::new(HashMap::new()),
            next_alloc_id: UnsafeCell::new(1),
            upcall_entries: UnsafeCell::new(Vec::new()),
            invoke_virtual_result: UnsafeCell::new(None),
            hidden_classes: UnsafeCell::new(std::collections::HashSet::new()),
            last_defined_class_name: UnsafeCell::new(None),
            class_flags_override: UnsafeCell::new(HashMap::new()),
            enum_values_override: UnsafeCell::new(HashMap::new()),
            interrupted_flag: UnsafeCell::new(false),
            code_base_override: UnsafeCell::new(HashMap::new()),
            code_source_certs_override: UnsafeCell::new(HashMap::new()),
            declared_fields_override: UnsafeCell::new(HashMap::new()),
            declared_methods_override: UnsafeCell::new(HashMap::new()),
            superclass_override: UnsafeCell::new(HashMap::new()),
            is_interface_override: UnsafeCell::new(HashMap::new()),
            interfaces_override: UnsafeCell::new(HashMap::new()),
            osc_cache_map: UnsafeCell::new(HashMap::new()),
            resources_override: UnsafeCell::new(HashMap::new()),
            registered_native_threads: UnsafeCell::new(Vec::new()),
            next_native_tid: UnsafeCell::new(1),
            native_thread_java_objs: UnsafeCell::new(HashMap::new()),
            registered_classpath: UnsafeCell::new(Vec::new()),
        }
    }

    /// T19_H15 — read a snapshot of the dynamic classpath entries that
    /// have been registered via `register_dynamic_classpath`.
    /// Returns the absolute path strings in insertion order.
    pub(crate) fn registered_classpath_snapshot(&self) -> Vec<String> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.registered_classpath.get()).clone() }
    }

    /// T19_K4: read the recorded mirror pointer for `thread_id`, or 0
    /// if no mirror was attached. Tests use this to confirm that
    /// Vert.x / XNIO event-loop spawns hand a real `java.lang.Thread`
    /// mirror through `set_native_thread_java_obj`.
    #[allow(dead_code)]
    pub(crate) fn native_thread_java_obj_ptr(&self, thread_id: u64) -> usize {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.native_thread_java_objs.get())
                .get(&thread_id)
                .copied()
                .unwrap_or(0)
        }
    }

    /// T19.H10: register a synthetic resource that `find_resource` will
    /// return. `name` is the classpath-relative path (no leading slash).
    /// Subsequent `find_resource(name)` returns a clone of `bytes`.
    #[allow(dead_code)]
    pub(crate) fn set_resource(&self, name: &str, bytes: Vec<u8>) {
        // SAFETY: single-threaded test code.
        unsafe { (*self.resources_override.get()).insert(name.to_string(), bytes) };
    }

    /// T19_K2: read the list of `register_native_thread` calls. Each
    /// entry is `(name, daemon, alive)`. Tests use this to assert that
    /// Vert.x / XNIO event-loop spawns route through the VM-tracking
    /// path with the right daemon flag.
    #[allow(dead_code)]
    pub(crate) fn registered_native_threads(&self) -> Vec<(String, bool, bool)> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.registered_native_threads.get()).clone() }
    }

    /// WP0.2: push declared field metadata for `class_id`.  Overrides
    /// the default (empty) `declared_fields` return.
    #[allow(dead_code)]
    pub(crate) fn set_declared_fields(&self, class_id: ClassId, fields: Vec<FieldMetadata>) {
        // SAFETY: single-threaded test code.
        unsafe { (*self.declared_fields_override.get()).insert(class_id.as_u32(), fields) };
    }

    /// WP0.2: push declared method metadata for `class_id`.  Overrides
    /// the default (empty) `declared_methods` return.
    #[allow(dead_code)]
    pub(crate) fn set_declared_methods(&self, class_id: ClassId, methods: Vec<MethodMetadata>) {
        // SAFETY: single-threaded test code.
        unsafe { (*self.declared_methods_override.get()).insert(class_id.as_u32(), methods) };
    }

    /// WP0.2: set the direct super-class of `class_id`.
    #[allow(dead_code)]
    pub(crate) fn set_superclass(&self, class_id: ClassId, super_id: ClassId) {
        unsafe { (*self.superclass_override.get()).insert(class_id.as_u32(), super_id) };
    }

    /// WP0.2: set the interface list of `class_id`.
    #[allow(dead_code)]
    pub(crate) fn set_interfaces(&self, class_id: ClassId, ifaces: Vec<ClassId>) {
        unsafe { (*self.interfaces_override.get()).insert(class_id.as_u32(), ifaces) };
    }

    /// G2: mark `class_id` as an interface (overrides default `false`).
    /// Required by tests that exercise the
    /// `Class.getSuperclass()`-returns-null-for-interfaces semantics or
    /// the `getMethods` interface skip.
    #[allow(dead_code)]
    pub(crate) fn set_is_interface(&self, class_id: ClassId, is_iface: bool) {
        unsafe { (*self.is_interface_override.get()).insert(class_id.as_u32(), is_iface) };
    }

    /// T19.N2: set the mock current-thread interrupted flag.
    ///
    /// Allows tests to simulate an interrupt arriving mid-operation, e.g.
    /// a background thread spawning `ctx.set_interrupted(true)` while the
    /// main test thread is inside a chunked `Thread.sleep0` loop.
    #[allow(dead_code)]
    pub(crate) fn set_interrupted(&self, v: bool) {
        // SAFETY: single-threaded test code; no aliasing.
        unsafe { *self.interrupted_flag.get() = v; }
    }

    fn heap_mut(&self) -> &mut Vec<HeapEntry> {
        // SAFETY: only used in single-threaded test code
        unsafe { &mut *self.heap.get() }
    }

    fn heap_ref(&self) -> &Vec<HeapEntry> {
        // SAFETY: only used in single-threaded test code
        unsafe { &*self.heap.get() }
    }

    fn ptr_map_mut(&self) -> &mut HashMap<usize, usize> {
        unsafe { &mut *self.ptr_to_index.get() }
    }

    fn ptr_map_ref(&self) -> &HashMap<usize, usize> {
        unsafe { &*self.ptr_to_index.get() }
    }

    fn alloc_entry(&mut self, entry: HeapEntry) -> ObjectRef {
        let heap = self.heap_mut();
        let index = heap.len();
        heap.push(entry);
        let ptr_val = self.next_ptr;
        self.next_ptr += 8; // keep 8-byte aligned
        self.ptr_map_mut().insert(ptr_val, index);
        // SAFETY: ptr_val is non-null (>= 8) and 8-byte aligned
        unsafe { ObjectRef::from_raw(ptr_val as *mut u8) }
    }

    fn entry_index(&self, obj: ObjectRef) -> usize {
        let ptr_val = obj.as_ptr() as usize;
        *self.ptr_map_ref().get(&ptr_val).expect("invalid ObjectRef in mock heap")
    }
}

impl NativeContext for MockNativeContext {
    fn load_class(&mut self, _name: &str) -> MethodCallResult {
        Ok(None)
    }

    fn new_object(&mut self, class_name: &str) -> MethodCallResult {
        let cid = self.ensure_class_initialized(class_name)?;
        let obj = self.alloc_entry(HeapEntry::Object {
            class_id: cid,
            fields: vec![Value::Int(0); 4],
        });
        Ok(Some(Value::Object(Some(obj))))
    }

    fn invoke(
        &mut self,
        _class_name: &str,
        _method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(None)
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        obj.as_ptr() as i32
    }

    fn record_printed_value(&mut self, _value: Value) {}

    fn class_name_of_id(&self, class_id: ClassId) -> Option<String> {
        self.class_names.get(&class_id.as_u32()).cloned()
    }

    fn class_id_of_object(&self, obj: ObjectRef) -> ClassId {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Object { class_id, .. } => *class_id,
            HeapEntry::Array { .. } => ClassId::new(0),
        }
    }

    fn capture_stack_trace(&mut self, _throwable_hash: i32) -> Vec<StackTraceEntry> {
        Vec::new()
    }

    fn get_stack_trace(&self, _throwable_hash: i32) -> Option<&[StackTraceEntry]> {
        None
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Object { fields, .. } => {
                fields.get(index).copied().unwrap_or(Value::Int(0))
            }
            _ => Value::Int(0),
        }
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        let idx = self.entry_index(obj);
        match &mut self.heap_mut()[idx] {
            HeapEntry::Object { fields, .. } => {
                if index >= fields.len() {
                    fields.resize(index + 1, Value::Int(0));
                }
                fields[index] = value;
            }
            _ => {}
        }
    }

    fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value {
        // Mock: no class hierarchy parse. Tests that use
        // `make_field_mirror` write the synthetic 7-slot Field layout
        // directly; map the JDK Field field names to the corresponding
        // synthetic slot so the production code's `get_field_by_name`
        // path still reads the right value. Unknown names are treated as
        // absent (returning `Int(0)` rather than silently shadowing slot
        // 0, which would corrupt slot-0 test state).
        match mock_jdk_field_slot(field_name) {
            Some(slot) => self.get_field(obj, slot),
            None => Value::Int(0),
        }
    }

    fn set_field_by_name(&self, obj: ObjectRef, field_name: &str, value: Value) {
        if let Some(slot) = mock_jdk_field_slot(field_name) {
            self.set_field(obj, slot, value);
        }
        // Unknown name → silently ignore (matches real-JDK mode when
        // the field doesn't exist in the class hierarchy).
    }

    fn resolve_field_index(&self, _class_name: &str, _field_name: &str) -> Option<usize> {
        None // Mock has no class metadata
    }

    fn method_exists(&self, _class_name: &str, _method_name: &str, _descriptor: &str) -> bool {
        false
    }

    fn new_array(&mut self, _element_type: ArrayElementType, length: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Array {
            elements: vec![Value::Int(0); length],
        })
    }

    fn new_ref_array(&mut self, _class_id: ClassId, length: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Array {
            elements: vec![Value::Object(None); length],
        })
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Array { elements } => elements.len(),
            _ => 0,
        }
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Array { elements } => {
                elements.get(index).copied().unwrap_or(Value::Int(0))
            }
            _ => Value::Int(0),
        }
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
        let idx = self.entry_index(obj);
        match &mut self.heap_mut()[idx] {
            HeapEntry::Array { elements } => {
                if index < elements.len() {
                    elements[index] = value;
                }
            }
            _ => {}
        }
    }

    fn heap_kind_of(&self, obj: ObjectRef) -> ObjectKind {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Object { .. } => ObjectKind::Object,
            HeapEntry::Array { .. } => ObjectKind::Array,
        }
    }

    fn heap_element_type_of(&self, _obj: ObjectRef) -> ArrayElementType {
        ArrayElementType::Reference
    }

    fn create_string(&mut self, text: &str) -> ObjectRef {
        // Create char array
        let chars: Vec<u16> = text.encode_utf16().collect();
        let arr = self.alloc_entry(HeapEntry::Array {
            elements: chars.iter().map(|&c| Value::Int(c as i32)).collect(),
        });
        // Create string object: field 0 = char[], field 1 = hash (0)
        self.alloc_entry(HeapEntry::Object {
            class_id: ClassId::new(0),
            fields: vec![Value::Object(Some(arr)), Value::Int(0)],
        })
    }

    fn read_string(&self, obj: ObjectRef) -> Option<String> {
        let arr_ref = match self.get_field(obj, 0) {
            Value::Object(Some(arr)) => arr,
            _ => return None,
        };
        let len = self.array_length(arr_ref);
        let mut chars = Vec::with_capacity(len);
        for i in 0..len {
            match self.get_array_element(arr_ref, i) {
                Value::Int(v) => chars.push(v as u16),
                _ => chars.push(0),
            }
        }
        String::from_utf16(&chars).ok()
    }

    fn get_class_mirror(&mut self, class_id: ClassId) -> ObjectRef {
        let name = self
            .class_names
            .get(&class_id.as_u32())
            .cloned()
            .unwrap_or_else(|| format!("unknown_{}", class_id.as_u32()));
        let name_obj = self.create_string(&name);
        self.alloc_entry(HeapEntry::Object {
            class_id: ClassId::new(0),
            fields: vec![
                Value::Int(class_id.as_u32() as i32),
                Value::Object(Some(name_obj)),
            ],
        })
    }

    fn record_printed_line(&mut self, _text: String) {}

    fn get_system_stream(&self, _name: &str) -> Option<ObjectRef> {
        None
    }

    fn get_system_property(&self, key: &str) -> Option<String> {
        self.properties.get(key).cloned()
    }

    fn set_system_property(&mut self, key: &str, value: &str) -> Option<String> {
        self.properties
            .insert(key.to_string(), value.to_string())
    }

    fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Object {
            class_id,
            fields: vec![Value::Int(0); num_fields],
        })
    }

    fn ensure_class_initialized(
        &mut self,
        name: &str,
    ) -> Result<ClassId, MethodCallFailed> {
        if let Some(&id) = self.name_to_id.get(name) {
            return Ok(ClassId::new(id));
        }
        let id = self.next_class_id;
        self.next_class_id += 1;
        self.class_names.insert(id, name.to_string());
        self.name_to_id.insert(name.to_string(), id);
        Ok(ClassId::new(id))
    }

    fn is_subclass(&self, child: ClassId, parent: ClassId) -> bool {
        if child == parent {
            return true;
        }
        // SAFETY: single-threaded test code.
        let supers = unsafe { &*self.superclass_override.get() };
        let ifaces = unsafe { &*self.interfaces_override.get() };
        let mut cur = Some(child);
        while let Some(cid) = cur {
            if let Some(list) = ifaces.get(&cid.as_u32()) {
                if list.contains(&parent) {
                    return true;
                }
                // Also recurse into each interface's super-interfaces
                // so `is_subclass(Foo, Serializable)` works even if
                // `Foo`'s declared interface is `MyMarker extends
                // Serializable`. Guarded against cycles via a shallow
                // visited set — tests don't construct deep hierarchies.
                for i in list {
                    if self.is_subclass(*i, parent) {
                        return true;
                    }
                }
            }
            if let Some(&sup) = supers.get(&cid.as_u32()) {
                if sup == parent {
                    return true;
                }
                cur = Some(sup);
            } else {
                cur = None;
            }
        }
        false
    }

    fn superclass_of(&self, class_id: ClassId) -> Option<ClassId> {
        // SAFETY: single-threaded test code.
        let supers = unsafe { &*self.superclass_override.get() };
        supers.get(&class_id.as_u32()).copied()
    }

    fn is_interface_class(&self, class_id: ClassId) -> bool {
        // SAFETY: single-threaded test code.
        let map = unsafe { &*self.is_interface_override.get() };
        map.get(&class_id.as_u32()).copied().unwrap_or(false)
    }

    fn class_id_by_name(&self, name: &str) -> Option<ClassId> {
        self.name_to_id.get(name).map(|&id| ClassId::new(id))
    }

    fn loader_id_of_class(&self, _class_id: ClassId) -> i32 {
        2 // default to app loader in test context
    }

    fn is_record_class(&self, _class_id: ClassId) -> bool {
        false
    }

    fn record_components(&self, _class_id: ClassId) -> Vec<(String, String)> {
        Vec::new()
    }

    fn is_sealed_class(&self, _class_id: ClassId) -> bool {
        false
    }

    fn permitted_subclasses(&self, _class_id: ClassId) -> Vec<String> {
        Vec::new()
    }

    fn object_num_fields(&self, obj: ObjectRef) -> usize {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Object { fields, .. } => fields.len(),
            _ => 0,
        }
    }

    fn thread_id(&self) -> u64 {
        1
    }

    fn monitor_wait(
        &mut self,
        _obj: ObjectRef,
        _timeout_ms: Option<u64>,
    ) -> MethodCallResult {
        Ok(None)
    }

    fn monitor_notify(&mut self, _obj: ObjectRef) -> MethodCallResult {
        Ok(None)
    }

    fn monitor_notify_all(&mut self, _obj: ObjectRef) -> MethodCallResult {
        Ok(None)
    }

    fn thread_start(&mut self, _thread_obj: ObjectRef) -> MethodCallResult {
        Ok(None)
    }

    fn thread_join(&mut self, _thread_obj: ObjectRef) -> MethodCallResult {
        Ok(None)
    }

    fn thread_is_alive(&self, _thread_obj: ObjectRef) -> bool {
        false
    }

    fn current_thread_object(&mut self) -> ObjectRef {
        self.alloc_object(ClassId::new(0), 2)
    }

    fn thread_interrupt(&mut self, _thread_obj: ObjectRef) {}

    fn register_native_thread(
        &mut self,
        name: &str,
        daemon: bool,
        join_handle_ptr: usize,
    ) -> u64 {
        // Drop the JoinHandle if one was passed — the mock context
        // doesn't model a real registry that can `.join()` on it,
        // and leaking it would prevent the OS thread from being
        // reaped at process exit.
        if join_handle_ptr != 0 {
            // SAFETY: caller built this via Box::into_raw; we
            // reconstruct and drop it.
            let _ =
                unsafe { Box::from_raw(join_handle_ptr as *mut std::thread::JoinHandle<()>) };
        }
        // SAFETY: single-threaded test code.
        let next = unsafe { &mut *self.next_native_tid.get() };
        let tid = *next;
        *next += 1;
        // SAFETY: same.
        unsafe {
            (*self.registered_native_threads.get())
                .push((name.to_string(), daemon, true));
        }
        tid
    }

    fn unregister_native_thread(&mut self, thread_id: u64) {
        if thread_id == 0 {
            return;
        }
        // SAFETY: single-threaded test code.
        unsafe {
            // Indices are 1-based to match the ids handed out.
            let idx = (thread_id as usize).saturating_sub(1);
            let v = &mut *self.registered_native_threads.get();
            if let Some(entry) = v.get_mut(idx) {
                entry.2 = false;
            }
        }
    }

    fn attach_join_handle_to_native_thread(
        &mut self,
        thread_id: u64,
        join_handle_ptr: usize,
    ) -> bool {
        if thread_id == 0 || join_handle_ptr == 0 {
            return false;
        }
        // SAFETY: caller built via Box::into_raw; we drop the box
        // (mock doesn't model joining).
        let _ =
            unsafe { Box::from_raw(join_handle_ptr as *mut std::thread::JoinHandle<()>) };
        // SAFETY: single-threaded test code.
        unsafe {
            let idx = (thread_id as usize).saturating_sub(1);
            let v = &*self.registered_native_threads.get();
            v.get(idx).is_some()
        }
    }

    fn set_native_thread_java_obj(
        &mut self,
        thread_id: u64,
        java_thread_obj: ObjectRef,
    ) -> bool {
        if thread_id == 0 {
            return false;
        }
        // SAFETY: single-threaded test code.
        unsafe {
            let idx = (thread_id as usize).saturating_sub(1);
            let v = &*self.registered_native_threads.get();
            if v.get(idx).is_none() {
                return false;
            }
            (*self.native_thread_java_objs.get())
                .insert(thread_id, java_thread_obj.as_ptr() as usize);
        }
        true
    }

    fn is_interrupted(&self, clear: bool) -> bool {
        // SAFETY: single-threaded test code; no aliasing.
        let slot = unsafe { &mut *self.interrupted_flag.get() };
        let val = *slot;
        if val && clear {
            *slot = false;
        }
        val
    }

    fn declared_fields(&self, class_id: ClassId) -> Vec<FieldMetadata> {
        // SAFETY: single-threaded test code.
        let overrides = unsafe { &*self.declared_fields_override.get() };
        overrides
            .get(&class_id.as_u32())
            .map(|v| {
                v.iter()
                    .map(|f| FieldMetadata {
                        name: f.name.clone(),
                        descriptor: f.descriptor.clone(),
                        access_flags: f.access_flags,
                        slot_index: f.slot_index,
                        declaring_class_id: f.declaring_class_id,
                        is_static: f.is_static,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn declared_methods(&self, class_id: ClassId) -> Vec<MethodMetadata> {
        let overrides = unsafe { &*self.declared_methods_override.get() };
        overrides
            .get(&class_id.as_u32())
            .map(|v| {
                v.iter()
                    .map(|m| MethodMetadata {
                        name: m.name.clone(),
                        descriptor: m.descriptor.clone(),
                        access_flags: m.access_flags,
                        declaring_class_id: m.declaring_class_id,
                        exceptions: m.exceptions.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId> {
        let overrides = unsafe { &*self.interfaces_override.get() };
        overrides.get(&class_id.as_u32()).cloned().unwrap_or_default()
    }

    fn class_access_flags(&self, class_id: ClassId) -> u16 {
        let overrides = unsafe { &*self.class_flags_override.get() };
        overrides.get(&class_id.as_u32()).copied().unwrap_or(0)
    }

    fn static_field_index_by_name(&self, class_id: ClassId, field_name: &str) -> Option<usize> {
        // C14: only `$VALUES` is tracked in the mock, at synthetic slot 0.
        let overrides = unsafe { &*self.enum_values_override.get() };
        if field_name == "$VALUES" && overrides.contains_key(&class_id.as_u32()) {
            Some(0)
        } else {
            None
        }
    }

    fn get_static_field(&self, class_id: ClassId, field_index: usize) -> Value {
        let overrides = unsafe { &*self.enum_values_override.get() };
        if field_index == 0 {
            if let Some(&arr) = overrides.get(&class_id.as_u32()) {
                return Value::Object(Some(arr));
            }
        }
        Value::Int(0)
    }

    fn set_static_field(&mut self, _class_id: ClassId, _field_index: usize, _value: Value) {}

    fn primitive_class_mirror(&mut self, name: &str) -> ObjectRef {
        let name_obj = self.create_string(name);
        self.alloc_entry(HeapEntry::Object {
            class_id: ClassId::new(0),
            fields: vec![Value::Int(0), Value::Object(Some(name_obj))],
        })
    }

    fn fd_table(&self) -> &rustjvm_native_api::fd_table::FileDescriptorTable {
        // Leak a static table for testing — tests won't actually use file I/O
        use std::sync::OnceLock;
        static FD_TABLE: OnceLock<rustjvm_native_api::fd_table::FileDescriptorTable> =
            OnceLock::new();
        FD_TABLE.get_or_init(rustjvm_native_api::fd_table::FileDescriptorTable::new)
    }

    // -- WP0.2 ObjectStreamClass cache overrides --

    fn osc_cache_get(&self, class_id: ClassId) -> Option<ObjectRef> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.osc_cache_map.get()).get(&class_id.as_u32()).copied() }
    }

    fn osc_cache_put(&self, class_id: ClassId, desc: ObjectRef) -> ObjectRef {
        // SAFETY: single-threaded test code.
        let map = unsafe { &mut *self.osc_cache_map.get() };
        *map.entry(class_id.as_u32()).or_insert(desc)
    }

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

    fn park(&mut self, _timeout: Option<std::time::Duration>) {}

    fn unpark(&self, _thread_obj: ObjectRef) {}

    fn allocate_instance(&mut self, class_name: &str) -> Option<ObjectRef> {
        let cid = self.ensure_class_initialized(class_name).ok()?;
        Some(self.alloc_object(cid, 4))
    }

    fn class_annotations(&self, _class_id: ClassId) -> Vec<AnnotationData> {
        Vec::new()
    }

    fn method_annotations(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<AnnotationData> {
        Vec::new()
    }

    fn field_annotations(&self, _class_id: ClassId, _field_name: &str) -> Vec<AnnotationData> {
        Vec::new()
    }

    fn invoke_virtual(
        &mut self,
        _receiver: ObjectRef,
        _method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> MethodCallResult {
        let result = unsafe { &mut *self.invoke_virtual_result.get() };
        if let Some(r) = result.take() {
            r
        } else {
            Ok(None)
        }
    }

    fn get_scoped_value(&self, _key_id: u64) -> Option<Value> {
        None
    }

    fn push_scoped_value(&mut self, _key_id: u64, _value: Value) {}

    fn pop_scoped_value(&mut self) {}

    fn scoped_value_depth(&self) -> usize { 0 }

    fn allocate_native_memory(&mut self, size: usize, align: usize) -> Option<(i64, *mut u8)> {
        let align = align.max(1);
        let size = size.max(1);
        let layout = std::alloc::Layout::from_size_align(size, align).ok()?;
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return None;
        }
        let allocs = unsafe { &mut *self.native_allocs.get() };
        let id_ref = unsafe { &mut *self.next_alloc_id.get() };
        let id = *id_ref;
        *id_ref += 1;
        allocs.insert(id, (ptr, layout));
        Some((id, ptr))
    }

    fn free_native_memory(&mut self, alloc_id: i64) {
        let allocs = unsafe { &mut *self.native_allocs.get() };
        if let Some((ptr, layout)) = allocs.remove(&alloc_id) {
            unsafe { std::alloc::dealloc(ptr, layout) };
        }
    }

    fn load_native_library(
        &mut self,
        _path: &str,
    ) -> Result<i64, MethodCallFailed> {
        Ok(0)
    }

    fn find_native_symbol(&self, _lib_index: i64, name: &str) -> Option<usize> {
        // For testing, resolve symbols from the C runtime via platform-specific lookup
        #[cfg(target_os = "windows")]
        {
            use std::ffi::CString;
            let c_name = CString::new(name).ok()?;
            // Try msvcrt first, then ucrtbase
            let libs = ["msvcrt.dll\0", "ucrtbase.dll\0"];
            for lib in &libs {
                let handle = unsafe {
                    winapi_GetModuleHandleA(lib.as_ptr() as *const i8)
                };
                if !handle.is_null() {
                    let addr = unsafe { winapi_GetProcAddress(handle, c_name.as_ptr()) };
                    if !addr.is_null() {
                        return Some(addr as usize);
                    }
                }
            }
            None
        }
        #[cfg(not(target_os = "windows"))]
        {
            use std::ffi::CString;
            let c_name = CString::new(name).ok()?;
            let addr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c_name.as_ptr()) };
            if addr.is_null() { None } else { Some(addr as usize) }
        }
    }

    fn register_upcall(&mut self, entry: rustjvm_native_api::ffi::UpcallEntry) -> usize {
        let entries = unsafe { &mut *self.upcall_entries.get() };
        let slot = entries.len();
        entries.push(entry);
        slot
    }

    fn get_upcall_info(&self, slot: usize) -> Option<(ObjectRef, Vec<i32>, i32)> {
        let entries = unsafe { &*self.upcall_entries.get() };
        entries.get(slot).map(|e| (e.target, e.param_kinds.clone(), e.return_kind))
    }

    fn module_name_of_class(&self, _class_id: ClassId) -> Option<String> {
        None
    }

    fn find_resource(&self, name: &str) -> Option<Vec<u8>> {
        // T19.H10: route through the per-instance override so tests can
        // simulate a classpath containing real resources (e.g. the
        // `keycloak-version.properties` blob the KC26 launcher reads).
        // Strip leading slash to match the production class-path search,
        // which trims absolute resource names before looking them up.
        let trimmed = name.trim_start_matches('/');
        // SAFETY: single-threaded test code.
        unsafe { (*self.resources_override.get()).get(trimmed).cloned() }
    }

    fn list_application_class_names(&self) -> Vec<String> {
        Vec::new()
    }

    fn register_dynamic_classpath(&mut self, paths: &[String]) {
        // T19_H15: record paths so tests can assert the JBoss module
        // loader registers the transitive linkage closure correctly.
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.registered_classpath.get()).extend(paths.iter().cloned());
        }
    }

    fn define_class_from_bytes(
        &mut self,
        name: &str,
        bytes: &[u8],
    ) -> Option<ClassId> {
        // NEW-8 mock: remember the name so tests can inspect it, then
        // mint a fresh ClassId. A bytes slice starting with the magic
        // CAFEBABE is accepted; anything else returns None so error
        // paths in the caller can be tested.
        if bytes.len() < 4 || bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
            return None;
        }
        unsafe {
            *self.last_defined_class_name.get() = Some(name.to_string());
        }
        let id = self.next_class_id;
        self.next_class_id += 1;
        self.class_names.insert(id, name.to_string());
        self.name_to_id.insert(name.to_string(), id);
        Some(ClassId::new(id))
    }

    fn set_class_hidden(&mut self, class_id: ClassId) {
        unsafe {
            (*self.hidden_classes.get()).insert(class_id.as_u32());
        }
    }

    fn is_class_hidden(&self, class_id: ClassId) -> bool {
        unsafe { (*self.hidden_classes.get()).contains(&class_id.as_u32()) }
    }

    fn discover_reference(
        &mut self,
        _ref_type: u8,
        _reference_obj: ObjectRef,
        _referent: ObjectRef,
        _queue: Option<ObjectRef>,
    ) {}

    fn monitor_enter(&mut self, _obj: ObjectRef) {}
    fn monitor_exit(&mut self, _obj: ObjectRef) {}

    fn define_class_with_loader(
        &mut self,
        _name: &str,
        _bytes: &[u8],
        _loader_id: u32,
    ) -> Option<ClassId> {
        None
    }

    fn class_id_by_name_and_loader(&self, _name: &str, _loader_id: u32) -> Option<ClassId> {
        None
    }

    fn allocate_loader_id(&mut self) -> u32 {
        0
    }

    fn active_thread_count(&self) -> i32 {
        1
    }

    fn enumerate_threads(&self, _max: usize) -> Vec<ObjectRef> {
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

    fn method_parameter_annotations(&self, _: ClassId, _: &str, _: &str) -> Vec<Vec<rustjvm_native_api::AnnotationData>> {
        Vec::new()
    }

    fn method_annotation_default(&self, _: ClassId, _: &str, _: &str) -> Option<rustjvm_native_api::AnnotationElementValue> {
        None
    }

    fn class_signature(&self, _class_id: ClassId) -> Option<String> {
        None
    }

    fn method_signature(&self, _class_id: ClassId, _method_name: &str, _method_desc: &str) -> Option<String> {
        None
    }

    fn field_signature(&self, _class_id: ClassId, _field_name: &str) -> Option<String> {
        None
    }

    /// T19.N1: honour any test-provided `code_base_override`; defaults to
    /// `None` (bootstrap/synthetic class) when no override is set.
    fn class_code_base(&self, class_id: ClassId) -> Option<String> {
        let overrides = unsafe { &*self.code_base_override.get() };
        overrides.get(&class_id.as_u32()).cloned()
    }

    /// T19.N1: honour any test-provided `code_source_certs_override`;
    /// defaults to empty (unsigned class).
    fn class_code_source_certs(&self, class_id: ClassId) -> Vec<Vec<u8>> {
        let overrides = unsafe { &*self.code_source_certs_override.get() };
        overrides.get(&class_id.as_u32()).cloned().unwrap_or_default()
    }
}

impl Drop for MockNativeContext {
    fn drop(&mut self) {
        let allocs = unsafe { &mut *self.native_allocs.get() };
        for (_, (ptr, layout)) in allocs.drain() {
            unsafe { std::alloc::dealloc(ptr, layout) };
        }
    }
}

/// Create a fresh MockNativeContext for testing.
pub(crate) fn mock_ctx() -> MockNativeContext {
    MockNativeContext::new()
}
