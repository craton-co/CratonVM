// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Shared test fixtures for native-collections integration tests.
//!
//! Provides a heap-backed `MockCtx` that implements `NativeContext`
//! richly enough to drive HashMap / ArrayList / LinkedHashMap / TreeMap
//! natives end-to-end. The bundled `cratonvm-native-api`
//! `test-mock::MockNativeContext` is intentionally minimal (it exists
//! to exercise *trait default impls*); collection natives, by contrast,
//! depend on real heap-backed arrays and field storage, so this module
//! grows the mock to a usable size.

#![allow(dead_code)]

use std::cell::UnsafeCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use cratonvm_native_api::{
    ffi::UpcallEntry, AnnotationData, AnnotationElementValue, FieldMetadata, MethodMetadata,
    NativeClassAccess, NativeContext, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeMethodRegistry, NativeSystemAccess, NativeThreadAccess,
    StackTraceEntry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

/// One heap entry — either a plain object (fields) or an array (elements).
enum HeapEntry {
    Object {
        class_id: ClassId,
        fields: Vec<Value>,
    },
    Array {
        elements: Vec<Value>,
    },
}

/// A heap-backed mock NativeContext.
///
/// Single-threaded by contract: uses `UnsafeCell` to satisfy the trait
/// methods that take `&self` but need interior mutation (`set_field`,
/// `set_array_element`).
pub struct MockCtx {
    heap: UnsafeCell<Vec<HeapEntry>>,
    /// raw-pointer → heap-index map for `ObjectRef` resolution.
    ptr_to_index: UnsafeCell<HashMap<usize, usize>>,
    /// class_id → name and reverse.
    class_names: HashMap<u32, String>,
    name_to_id: HashMap<String, u32>,
    class_interfaces: UnsafeCell<HashMap<u32, Vec<ClassId>>>,
    lambda_functional_interfaces: UnsafeCell<HashMap<u32, String>>,
    lambda_proxy_hosts: UnsafeCell<HashMap<u32, String>>,
    next_class_id: u32,
    next_ptr: usize,
    /// Stable identity hashes — assigned on first probe and remembered.
    identity_hashes: UnsafeCell<HashMap<usize, i32>>,
    next_identity: UnsafeCell<i32>,
    /// Programmable single-shot result for `invoke_virtual`.
    invoke_virtual_result: UnsafeCell<Option<MethodCallResult>>,
    /// Programmable result sequence for tests that need multiple callbacks
    /// within one native operation (for example hashCode followed by equals).
    invoke_virtual_results: UnsafeCell<VecDeque<MethodCallResult>>,
    /// Append-only log of every `invoke_virtual` call made on this
    /// context. Tests that need to observe per-entry callbacks (e.g.
    /// `forEach`) consult this to recover the visit order without
    /// re-implementing the callable receiver. Each entry is
    /// `(receiver_ptr, method_name, descriptor, args)`.
    invoke_virtual_log: UnsafeCell<Vec<(usize, String, String, Vec<Value>)>>,
    /// Per-native-call roots, mirroring `JvmThread::native_pin_roots`.
    native_pin_roots: UnsafeCell<Vec<ObjectRef>>,
    /// Test hook: simulate a moving GC during every Java callback by relocating
    /// all currently pinned roots before `invoke_virtual` returns.
    relocate_pins_on_invoke: UnsafeCell<bool>,
    /// Test hook: simulate a moving GC immediately before every heap
    /// allocation. This exercises native graph builders that must pin raw
    /// arguments and intermediate objects across `alloc_*` calls.
    relocate_pins_on_alloc: bool,
}

impl Default for MockCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide pointer-namespace counter.
///
/// The `native-collections` crate stashes its LinkedList / LinkedHashMap
/// / TreeMap / TreeSet side-table state in `static OnceLock<Mutex<...>>`
/// maps that outlive any single test. To stop two tests in the same
/// integration binary from clobbering each other's overlay entries, each
/// `MockCtx` claims a fresh, non-overlapping address window. The window
/// is 2^32 bytes wide and starts above `1 << 40` to keep the addresses
/// numerically distinct from any plausible real pointer (we never
/// dereference them — `ObjectRef` is only ever compared and indexed
/// through our local map).
static NEXT_NAMESPACE: AtomicUsize = AtomicUsize::new(1);
/// Per-process identity-hash base. C21's overlay keys are identity-hash
/// codes (i32); without a per-MockCtx base, two contexts both start at
/// 1 and would alias overlay entries.
static NEXT_HASH_BASE: AtomicI32 = AtomicI32::new(1);

impl MockCtx {
    pub fn new() -> Self {
        // Carve out a fresh 4 GiB address window for this context.
        // 1 << 40 (1 TiB) base is comfortably above anything a real
        // allocator hands out in a test process, and the +1 << 32
        // stride per ctx guarantees no two windows overlap.
        let ns = NEXT_NAMESPACE.fetch_add(1, Ordering::Relaxed);
        let ptr_base = (1usize << 40).wrapping_add(ns << 32);
        // Identity-hash base: claim 1 << 24 codes per ctx so tests that
        // allocate hundreds of objects cannot wrap into another ctx's
        // range (an i32 has ~127 million such windows before exhausting).
        let hash_base = NEXT_HASH_BASE.fetch_add(1 << 24, Ordering::Relaxed);
        let mut class_names = HashMap::new();
        class_names.insert(0, "java/lang/Object".to_string());
        let mut name_to_id = HashMap::new();
        name_to_id.insert("java/lang/Object".to_string(), 0);
        Self {
            heap: UnsafeCell::new(Vec::new()),
            ptr_to_index: UnsafeCell::new(HashMap::new()),
            class_names,
            name_to_id,
            class_interfaces: UnsafeCell::new(HashMap::new()),
            lambda_functional_interfaces: UnsafeCell::new(HashMap::new()),
            lambda_proxy_hosts: UnsafeCell::new(HashMap::new()),
            next_class_id: 1,
            next_ptr: ptr_base,
            identity_hashes: UnsafeCell::new(HashMap::new()),
            next_identity: UnsafeCell::new(hash_base),
            invoke_virtual_result: UnsafeCell::new(None),
            invoke_virtual_results: UnsafeCell::new(VecDeque::new()),
            invoke_virtual_log: UnsafeCell::new(Vec::new()),
            native_pin_roots: UnsafeCell::new(Vec::new()),
            relocate_pins_on_invoke: UnsafeCell::new(false),
            relocate_pins_on_alloc: false,
        }
    }

    /// Snapshot the `invoke_virtual` call log. Returned in invocation order.
    pub fn invoke_virtual_log(&self) -> Vec<(usize, String, String, Vec<Value>)> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.invoke_virtual_log.get()).clone() }
    }

    /// Clear the `invoke_virtual` call log.
    pub fn clear_invoke_virtual_log(&self) {
        // SAFETY: single-threaded test code.
        unsafe { (*self.invoke_virtual_log.get()).clear() }
    }

    pub fn set_invoke_virtual_result(&self, r: MethodCallResult) {
        // SAFETY: single-threaded test code.
        unsafe {
            *self.invoke_virtual_result.get() = Some(r);
        }
    }

    pub fn set_invoke_virtual_results(&self, results: Vec<MethodCallResult>) {
        // SAFETY: single-threaded test code.
        unsafe {
            *self.invoke_virtual_results.get() = results.into();
        }
    }

    pub fn set_relocate_pins_on_invoke(&self, enabled: bool) {
        // SAFETY: single-threaded test code.
        unsafe {
            *self.relocate_pins_on_invoke.get() = enabled;
        }
    }

    pub fn set_relocate_pins_on_alloc(&mut self, enabled: bool) {
        self.relocate_pins_on_alloc = enabled;
    }

    pub fn set_class_interfaces(&self, class_id: ClassId, interfaces: Vec<ClassId>) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.class_interfaces.get()).insert(class_id.as_u32(), interfaces);
        }
    }

    /// Registers hidden-lambda metadata without adding it to the class table.
    pub fn set_lambda_proxy_metadata(
        &self,
        class_id: ClassId,
        functional_interface: &str,
        host: &str,
    ) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.lambda_functional_interfaces.get())
                .insert(class_id.as_u32(), functional_interface.to_string());
            (*self.lambda_proxy_hosts.get()).insert(class_id.as_u32(), host.to_string());
        }
    }

    fn heap_mut(&self) -> &mut Vec<HeapEntry> {
        // SAFETY: single-threaded test code.
        unsafe { &mut *self.heap.get() }
    }
    fn heap_ref(&self) -> &Vec<HeapEntry> {
        // SAFETY: single-threaded test code.
        unsafe { &*self.heap.get() }
    }
    fn ptr_map_mut(&self) -> &mut HashMap<usize, usize> {
        // SAFETY: single-threaded test code.
        unsafe { &mut *self.ptr_to_index.get() }
    }
    fn ptr_map_ref(&self) -> &HashMap<usize, usize> {
        // SAFETY: single-threaded test code.
        unsafe { &*self.ptr_to_index.get() }
    }

    fn alloc_entry(&mut self, entry: HeapEntry) -> ObjectRef {
        let heap = self.heap_mut();
        let idx = heap.len();
        heap.push(entry);
        let ptr_val = self.next_ptr;
        self.next_ptr += 8;
        self.ptr_map_mut().insert(ptr_val, idx);
        // SAFETY: ptr_val >= 8 and 8-byte aligned.
        unsafe { ObjectRef::from_raw(ptr_val as *mut u8) }
    }

    fn entry_index(&self, obj: ObjectRef) -> Option<usize> {
        self.ptr_map_ref().get(&(obj.as_ptr() as usize)).copied()
    }

    /// Forge a fresh ObjectRef at a synthetic address, bypassing the
    /// heap-index map. Used by the GC-relocation harness to fabricate a
    /// "post-GC" pointer with the same numeric value as a previously-
    /// allocated object, simulating relocation aliasing.
    pub fn forge_object_ref(&self, ptr_val: usize) -> ObjectRef {
        debug_assert!(ptr_val % 8 == 0 && ptr_val >= 8);
        // SAFETY: caller picks a non-null, 8-aligned value.
        unsafe { ObjectRef::from_raw(ptr_val as *mut u8) }
    }

    /// Reuse the next allocation address so the next `alloc_*` call
    /// lands at `ptr_val`. Used by the GC-relocation harness.
    pub fn set_next_ptr(&mut self, ptr_val: usize) {
        assert!(ptr_val % 8 == 0 && ptr_val >= 8);
        self.next_ptr = ptr_val;
    }

    /// Snapshot the next pointer the mock will hand out. Useful for
    /// the GC-relocation test to capture an address before allocating
    /// a fresh object on top of it.
    pub fn peek_next_ptr(&self) -> usize {
        self.next_ptr
    }

    /// Convenience wrapper around `NativeContext::alloc_object` for tests
    /// that don't care about field count. Used by the GC-relocation
    /// harness (task #13) where the harness just needs distinct
    /// `ObjectRef`s on which to exercise identity-hash stability.
    pub fn alloc_object_simple(&mut self, class_id: u32) -> ObjectRef {
        <Self as NativeHeapAccess>::alloc_object(self, ClassId::new(class_id), 0)
    }

    pub fn set_object_class_id_for_test(&mut self, obj: ObjectRef, new_class_id: ClassId) {
        let idx = self
            .entry_index(obj)
            .expect("set_object_class_id_for_test on invalid ObjectRef");
        match &mut self.heap_mut()[idx] {
            HeapEntry::Object { class_id, .. } => *class_id = new_class_id,
            HeapEntry::Array { .. } => panic!("set_object_class_id_for_test on array"),
        }
    }

    /// Move `old` to a fresh address while preserving its identity-hash
    /// mapping. Returns the new `ObjectRef`.
    ///
    /// Models a moving / compacting GC: the heap entry stays the same
    /// (so field/array reads still find the right backing storage), but
    /// the object's pointer value changes. The identity-hash table is
    /// updated so the *same* identity hash code that `old` produced is
    /// also produced for the new ObjectRef — that is the contract a
    /// real moving GC honors by copying the hash word with the object
    /// header. This lets the test prove that overlay tables keyed by
    /// `identity_hash_code` (C21) survive relocation, while overlays
    /// keyed by raw pointer would lose state.
    pub fn relocate_object(&mut self, old: ObjectRef) -> ObjectRef {
        let old_key = old.as_ptr() as usize;
        let entry_idx = *self
            .ptr_map_ref()
            .get(&old_key)
            .expect("relocate_object: stale ObjectRef");
        // Mint a fresh slot.
        let new_ptr_val = self.next_ptr;
        self.next_ptr += 8;
        // Repoint the heap-index map and remove the old binding so
        // subsequent reads against `old` cleanly fail.
        self.ptr_map_mut().insert(new_ptr_val, entry_idx);
        self.ptr_map_mut().remove(&old_key);
        // Copy the identity hash so the new ObjectRef yields the same
        // hash code as the relocated one did.
        // SAFETY: single-threaded test code.
        let hash_map = unsafe { &mut *self.identity_hashes.get() };
        let existing = hash_map.get(&old_key).copied();
        if let Some(h) = existing {
            hash_map.insert(new_ptr_val, h);
            hash_map.remove(&old_key);
        }
        // SAFETY: new_ptr_val is non-null (>= 8) and 8-aligned.
        unsafe { ObjectRef::from_raw(new_ptr_val as *mut u8) }
    }

    fn relocate_native_pins(&mut self) {
        // SAFETY: single-threaded test code.
        let old_roots = unsafe { (*self.native_pin_roots.get()).clone() };
        let mut moved = HashMap::new();
        let mut new_roots = Vec::with_capacity(old_roots.len());
        for root in old_roots {
            let key = root.as_ptr() as usize;
            if let Some(new_root) = moved.get(&key).copied() {
                new_roots.push(new_root);
            } else if self.entry_index(root).is_some() {
                let new_root = self.relocate_object(root);
                moved.insert(key, new_root);
                new_roots.push(new_root);
            } else {
                new_roots.push(root);
            }
        }
        if !moved.is_empty() {
            for entry in self.heap_mut() {
                let values = match entry {
                    HeapEntry::Object { fields, .. } => fields,
                    HeapEntry::Array { elements } => elements,
                };
                for value in values {
                    if let Value::Object(Some(obj)) = value {
                        if let Some(new_obj) = moved.get(&(obj.as_ptr() as usize)).copied() {
                            *obj = new_obj;
                        }
                    }
                }
            }
        }
        // The production collectors also hand the pointer map to the native
        // collections' overlay tables (`lhm_overlay`, the TreeMap/TreeSet array
        // tables, ...), which hold `ObjectRef`s in process-global Rust state
        // that no heap walk can reach. Without this the mock would report
        // corruption for maps whose state lives in an overlay even when the
        // native is perfectly rooted — and, worse, would hide a native that
        // wrongly relies on the overlay staying pointer-stable.
        if !moved.is_empty() {
            let pointer_map: cratonvm_types::PointerMap = moved
                .iter()
                .map(|(old, new)| (*old, new.as_ptr() as usize))
                .collect();
            cratonvm_native_collections::gc_update_collection_overlay_refs(&pointer_map);
        }
        // SAFETY: single-threaded test code.
        unsafe {
            *self.native_pin_roots.get() = new_roots;
        }
    }
}

impl cratonvm_native_api::NativeClassAccess for MockCtx {
    // ------------------------------------------------------------------
    // Class loading / dispatch — synthetic class registry only.
    // ------------------------------------------------------------------

    fn load_class(&mut self, _name: &str) -> MethodCallResult {
        Ok(None)
    }

    fn class_name_of_id(&self, class_id: ClassId) -> Option<String> {
        self.class_names.get(&class_id.as_u32()).cloned()
    }

    fn class_id_of_object(&self, obj: ObjectRef) -> ClassId {
        match self.entry_index(obj) {
            Some(idx) => match &self.heap_ref()[idx] {
                HeapEntry::Object { class_id, .. } => *class_id,
                HeapEntry::Array { .. } => ClassId::new(0),
            },
            None => ClassId::new(0),
        }
    }
    fn method_exists(&self, _c: &str, _m: &str, _d: &str) -> bool {
        false
    }

    fn ensure_class_initialized(&mut self, name: &str) -> Result<ClassId, MethodCallFailed> {
        if let Some(&id) = self.name_to_id.get(name) {
            return Ok(ClassId::new(id));
        }
        let id = self.next_class_id;
        self.next_class_id += 1;
        self.class_names.insert(id, name.to_string());
        self.name_to_id.insert(name.to_string(), id);
        Ok(ClassId::new(id))
    }

    fn is_subclass(&self, c: ClassId, p: ClassId) -> bool {
        c == p
    }
    fn superclass_of(&self, _c: ClassId) -> Option<ClassId> {
        None
    }
    fn class_id_by_name(&self, name: &str) -> Option<ClassId> {
        self.name_to_id.get(name).copied().map(ClassId::new)
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
    fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.class_interfaces.get())
                .get(&class_id.as_u32())
                .cloned()
                .unwrap_or_default()
        }
    }
    fn lambda_functional_interface(&self, class_id: ClassId) -> Option<String> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.lambda_functional_interfaces.get())
                .get(&class_id.as_u32())
                .cloned()
        }
    }
    fn lambda_proxy_host(&self, class_id: ClassId) -> Option<String> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.lambda_proxy_hosts.get())
                .get(&class_id.as_u32())
                .cloned()
        }
    }
    fn class_access_flags(&self, _c: ClassId) -> u16 {
        0
    }
    fn primitive_class_mirror(&mut self, name: &str) -> ObjectRef {
        let name_obj = self.create_string(name);
        self.alloc_entry(HeapEntry::Object {
            class_id: ClassId::new(0),
            fields: vec![Value::Int(0), Value::Object(Some(name_obj))],
        })
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
    fn check_deep_reflection_access(&self, _a: ClassId, _t: ClassId) -> Result<(), String> {
        Ok(())
    }
}

impl cratonvm_native_api::NativeInvokeAccess for MockCtx {
    fn invoke(&mut self, class: &str, method: &str, _d: &str, a: &[Value]) -> MethodCallResult {
        // Just enough of the JDK static boxers for the natives that
        // round-trip primitives through `Integer.valueOf` / `Long.valueOf`
        // (e.g. TreeMap's `firstKey` returns a boxed primitive built via
        // `ctx.invoke`). The boxed object's slot 0 carries the wrapped
        // primitive, matching the layout that `tree_key_from_value` (and
        // its peers) probe for.
        match (class, method) {
            ("java/lang/Integer", "valueOf") => {
                if let Some(Value::Int(v)) = a.first().copied() {
                    let cid = self.ensure_class_initialized("java/lang/Integer")?;
                    let obj = self.alloc_object(cid, 1);
                    self.set_field(obj, 0, Value::Int(v));
                    return Ok(Some(Value::Object(Some(obj))));
                }
            }
            ("java/lang/Long", "valueOf") => {
                if let Some(Value::Long(v)) = a.first().copied() {
                    let cid = self.ensure_class_initialized("java/lang/Long")?;
                    let obj = self.alloc_object(cid, 1);
                    self.set_field(obj, 0, Value::Long(v));
                    return Ok(Some(Value::Object(Some(obj))));
                }
            }
            // The other int-field boxers (Character / Byte / Short / Boolean):
            // slot 0 carries the Int value, class_id carries the wrapper name —
            // exactly the layout `tree_key_to_value` produces, so a reboxed
            // TreeMap key round-trips to the correct wrapper class.
            (
                "java/lang/Character" | "java/lang/Byte" | "java/lang/Short" | "java/lang/Boolean",
                "valueOf",
            ) => {
                if let Some(Value::Int(v)) = a.first().copied() {
                    let cid = self.ensure_class_initialized(class)?;
                    let obj = self.alloc_object(cid, 1);
                    self.set_field(obj, 0, Value::Int(v));
                    return Ok(Some(Value::Object(Some(obj))));
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn invoke_virtual(&mut self, r: ObjectRef, m: &str, d: &str, a: &[Value]) -> MethodCallResult {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.invoke_virtual_log.get()).push((
                r.as_ptr() as usize,
                m.to_string(),
                d.to_string(),
                a.to_vec(),
            ));
        }
        if m == "compare"
            && d == "(Ljava/lang/Object;Ljava/lang/Object;)I"
            && self
                .class_name_arc_of_id(self.class_id_of_object(r))
                .as_deref()
                == Some("test/LiquibaseTieComparator")
        {
            let order_of = |ctx: &MockCtx, v: Value| -> i32 {
                match v {
                    Value::Object(Some(o)) => match ctx.get_field(o, 0) {
                        Value::Int(order) => order,
                        _ => 0,
                    },
                    _ => 0,
                }
            };
            let left = a.first().copied().unwrap_or(Value::Object(None));
            let right = a.get(1).copied().unwrap_or(Value::Object(None));
            let cmp = order_of(self, left).cmp(&order_of(self, right)) as i32;
            return Ok(Some(Value::Int(if cmp == 0 { 1 } else { cmp })));
        }
        match (m, d) {
            ("complete", "(Ljava/lang/Object;)Z") => {
                let val = a.first().copied().unwrap_or(Value::Object(None));
                let stored = if matches!(val, Value::Object(None)) {
                    let cid = self
                        .ensure_class_initialized(
                            "java/util/concurrent/CompletableFuture$AltResult",
                        )
                        .unwrap();
                    let alt = self.alloc_object(cid, 1);
                    self.set_field(alt, 0, Value::Object(None));
                    Value::Object(Some(alt))
                } else {
                    val
                };
                self.set_field(r, 0, stored);
                return Ok(Some(Value::Int(1)));
            }
            ("obtrudeException", "(Ljava/lang/Throwable;)V") => {
                let exc = a.first().copied().unwrap_or(Value::Object(None));
                let cid = self
                    .ensure_class_initialized("java/util/concurrent/CompletableFuture$AltResult")
                    .unwrap();
                let alt = self.alloc_object(cid, 1);
                self.set_field(alt, 0, exc);
                self.set_field(r, 0, Value::Object(Some(alt)));
                return Ok(None);
            }
            ("postComplete", "()V") => return Ok(None),
            _ => {}
        }
        let queue = unsafe { &mut *self.invoke_virtual_results.get() };
        let slot = unsafe { &mut *self.invoke_virtual_result.get() };
        let result = if let Some(r) = queue.pop_front() {
            r
        } else if let Some(r) = slot.take() {
            r
        } else {
            Ok(None)
        };
        if unsafe { *self.relocate_pins_on_invoke.get() } {
            self.relocate_native_pins();
        }
        result
    }
}

impl cratonvm_native_api::NativeHeapAccess for MockCtx {
    fn new_object(&mut self, class_name: &str) -> MethodCallResult {
        let cid = self.ensure_class_initialized(class_name)?;
        let obj = self.alloc_entry(HeapEntry::Object {
            class_id: cid,
            fields: vec![Value::Object(None); 4],
        });
        Ok(Some(Value::Object(Some(obj))))
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        let k = obj.as_ptr() as usize;
        // SAFETY: single-threaded test code.
        let map = unsafe { &mut *self.identity_hashes.get() };
        if let Some(&h) = map.get(&k) {
            return h;
        }
        let counter = unsafe { &mut *self.next_identity.get() };
        let h = *counter;
        *counter = counter.wrapping_add(1);
        map.insert(k, h);
        h
    }

    // ------------------------------------------------------------------
    // Field access — real heap-backed.
    // ------------------------------------------------------------------

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        let idx = match self.entry_index(obj) {
            Some(i) => i,
            None => return Value::Object(None),
        };
        match &self.heap_ref()[idx] {
            HeapEntry::Object { fields, .. } => {
                fields.get(index).copied().unwrap_or(Value::Object(None))
            }
            _ => Value::Object(None),
        }
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        let idx = match self.entry_index(obj) {
            Some(i) => i,
            None => return,
        };
        if let HeapEntry::Object { fields, .. } = &mut self.heap_mut()[idx] {
            if index >= fields.len() {
                fields.resize(index + 1, Value::Object(None));
            }
            fields[index] = value;
        }
    }

    fn get_field_by_name(&self, obj: ObjectRef, _f: &str) -> Value {
        // The collections code only writes through real slots in the
        // synthetic layout; `*_by_name` is mostly a write-through for
        // alternative real-JDK slot names. Default-read to null so the
        // synthetic path's `lhm_is_access_order` etc. fall back to the
        // overlay value instead of being shadowed by a false `Int(1)`.
        let _ = obj;
        Value::Object(None)
    }
    fn set_field_by_name(&self, _o: ObjectRef, _f: &str, _v: Value) {}

    /// Hand back slot indices that align with the real-JDK
    /// `java.util.ArrayList` / `ArrayList$Itr` layout so the natives'
    /// "real-JDK path" runs end-to-end instead of falling through to
    /// their synthetic fallback. The fallback collapses `cursor` and
    /// `lastRet` onto the same slot, which infinite-loops the
    /// iterator: every `next()` writes `lastRet = cursor` AFTER writing
    /// `cursor += 1`, so the next `hasNext()` sees the stale cursor and
    /// returns true forever.
    ///
    /// The slot indices below are the JDK-25 layouts. Other classes
    /// not enumerated here continue to receive `None` (the synthetic
    /// fallback), which is correct for HashMap / LHM / TreeMap natives
    /// that read named state through their side-table overlays anyway.
    // Drive-by test fix (cce0079): the trait gained
    // `resolve_field_index_by_class_id` without this mock being updated —
    // the integration-test target did not compile on dev.
    fn resolve_field_index_by_class_id(&self, _c: ClassId, _f: &str) -> Option<usize> {
        None
    }
    fn resolve_field_index(&self, class_name: &str, field_name: &str) -> Option<usize> {
        match (class_name, field_name) {
            // Real-JDK ArrayList: modCount, elementData, size.
            ("java/util/ArrayList", "elementData") => Some(1),
            ("java/util/ArrayList", "size") => Some(2),
            // Real-JDK ArrayList$Itr: cursor, lastRet, expectedModCount, this$0.
            ("java/util/ArrayList$Itr", "cursor") => Some(0),
            ("java/util/ArrayList$Itr", "lastRet") => Some(1),
            ("java/util/ArrayList$Itr", "this$0") => Some(3),
            _ => None,
        }
    }

    // ------------------------------------------------------------------
    // Arrays — real heap-backed.
    // ------------------------------------------------------------------

    fn new_array(&mut self, _et: ArrayElementType, length: usize) -> ObjectRef {
        if self.relocate_pins_on_alloc {
            self.relocate_native_pins();
        }
        self.alloc_entry(HeapEntry::Array {
            elements: vec![Value::Int(0); length],
        })
    }

    fn new_ref_array(&mut self, _c: ClassId, length: usize) -> ObjectRef {
        if self.relocate_pins_on_alloc {
            self.relocate_native_pins();
        }
        self.alloc_entry(HeapEntry::Array {
            elements: vec![Value::Object(None); length],
        })
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        match self.entry_index(obj) {
            Some(idx) => match &self.heap_ref()[idx] {
                HeapEntry::Array { elements } => elements.len(),
                _ => 0,
            },
            None => 0,
        }
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
        let idx = match self.entry_index(obj) {
            Some(i) => i,
            None => return Value::Object(None),
        };
        match &self.heap_ref()[idx] {
            HeapEntry::Array { elements } => {
                elements.get(index).copied().unwrap_or(Value::Object(None))
            }
            _ => Value::Object(None),
        }
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
        let idx = match self.entry_index(obj) {
            Some(i) => i,
            None => return,
        };
        if let HeapEntry::Array { elements } = &mut self.heap_mut()[idx] {
            if index < elements.len() {
                elements[index] = value;
            }
        }
    }

    fn heap_kind_of(&self, obj: ObjectRef) -> ObjectKind {
        match self.entry_index(obj) {
            Some(idx) => match &self.heap_ref()[idx] {
                HeapEntry::Object { .. } => ObjectKind::Object,
                HeapEntry::Array { .. } => ObjectKind::Array,
            },
            None => ObjectKind::Object,
        }
    }

    fn heap_element_type_of(&self, _o: ObjectRef) -> ArrayElementType {
        ArrayElementType::Reference
    }

    // ------------------------------------------------------------------
    // Strings — char[] backed.
    // ------------------------------------------------------------------

    fn create_string(&mut self, text: &str) -> ObjectRef {
        let chars: Vec<u16> = text.encode_utf16().collect();
        let arr = self.alloc_entry(HeapEntry::Array {
            elements: chars.iter().map(|&c| Value::Int(c as i32)).collect(),
        });
        self.alloc_entry(HeapEntry::Object {
            class_id: ClassId::new(0),
            fields: vec![Value::Object(Some(arr)), Value::Int(0)],
        })
    }

    fn read_string(&self, obj: ObjectRef) -> Option<String> {
        let arr_ref = match self.get_field(obj, 0) {
            Value::Object(Some(a)) => a,
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

    fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        if self.relocate_pins_on_alloc {
            self.relocate_native_pins();
        }
        self.alloc_entry(HeapEntry::Object {
            class_id,
            fields: vec![Value::Object(None); num_fields],
        })
    }

    fn object_num_fields(&self, obj: ObjectRef) -> usize {
        match self.entry_index(obj) {
            Some(idx) => match &self.heap_ref()[idx] {
                HeapEntry::Object { fields, .. } => fields.len(),
                _ => 0,
            },
            None => 0,
        }
    }

    fn heap_allocated_bytes(&self) -> usize {
        0
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
    fn allocate_instance(&mut self, class_name: &str) -> Option<ObjectRef> {
        let cid = self.ensure_class_initialized(class_name).ok()?;
        Some(self.alloc_object(cid, 4))
    }

    fn pin_native_root(&mut self, obj: ObjectRef) -> usize {
        // SAFETY: single-threaded test code.
        let roots = unsafe { &mut *self.native_pin_roots.get() };
        let idx = roots.len();
        roots.push(obj);
        idx
    }

    fn read_native_pin(&self, handle: usize, fallback: ObjectRef) -> ObjectRef {
        // SAFETY: single-threaded test code.
        unsafe {
            (&*self.native_pin_roots.get())
                .get(handle)
                .copied()
                .unwrap_or(fallback)
        }
    }

    fn unpin_native_roots(&mut self, base: usize) {
        // SAFETY: single-threaded test code.
        let roots = unsafe { &mut *self.native_pin_roots.get() };
        if base < roots.len() {
            roots.truncate(base);
        }
    }
    fn discover_reference(&mut self, _t: u8, _r: ObjectRef, _f: ObjectRef, _q: Option<ObjectRef>) {}
}

impl cratonvm_native_api::NativeThreadAccess for MockCtx {
    // ------------------------------------------------------------------
    // Threading — single-threaded stubs.
    // ------------------------------------------------------------------

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
        self.alloc_object(ClassId::new(0), 2)
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

impl cratonvm_native_api::NativeExceptionAccess for MockCtx {
    fn capture_stack_trace(&mut self, _h: i32) -> Vec<StackTraceEntry> {
        Vec::new()
    }
    fn get_stack_trace(&self, _h: i32) -> Option<Vec<StackTraceEntry>> {
        None
    }
}

impl cratonvm_native_api::NativeGpuAccess for MockCtx {}

impl cratonvm_native_api::NativeSystemAccess for MockCtx {
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

    fn try_ensure_synthetic_class(
        &mut self,
        name: &str,
        _num_fields: usize,
    ) -> Result<ClassId, cratonvm_native_api::ClassIdentityError> {
        // The mock carries no compatibility policy, so it never refuses: it
        // stands in for `Compatible` mode. It overrode the infallible spelling
        // until JDK-only wave 2 step 3 deleted that (2026-08-10).
        Ok(self
            .ensure_class_initialized(name)
            .unwrap_or(ClassId::new(0)))
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
        use std::sync::OnceLock;
        static T: OnceLock<cratonvm_native_api::fd_table::FileDescriptorTable> = OnceLock::new();
        T.get_or_init(cratonvm_native_api::fd_table::FileDescriptorTable::new)
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

// ------------------------------------------------------------------
// Test helpers — register the natives once per test, then dispatch.
// ------------------------------------------------------------------

/// Build a registry pre-populated with `register_collections_natives`.
pub fn build_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_collections::register_collections_natives(&mut r);
    r
}

/// Look up + invoke a native by its (class, method, descriptor) triple.
///
/// Panics on a missing native — keeps tests terse; the registration-
/// completeness suite in `lib.rs` already guards typos.
pub fn call(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    method: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    let cb = reg
        .find(class, method, desc)
        .unwrap_or_else(|| panic!("native not registered: {class}.{method}{desc}"));
    cb(ctx, args)
}

/// Allocate a fresh empty `java/util/ArrayList` and run its native `<init>`.
pub fn new_arraylist(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> ObjectRef {
    let cid = ctx.ensure_class_initialized("java/util/ArrayList").unwrap();
    let al = ctx.alloc_object(cid, 4);
    call(
        reg,
        ctx,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    al
}

/// Allocate a fresh empty `java/util/HashMap` and run its native `<init>`.
pub fn new_hashmap(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> ObjectRef {
    let cid = ctx.ensure_class_initialized("java/util/HashMap").unwrap();
    let hm = ctx.alloc_object(cid, 6);
    call(
        reg,
        ctx,
        "java/util/HashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(hm))],
    )
    .unwrap();
    hm
}

/// Allocate a fresh empty `java/util/concurrent/ConcurrentHashMap`.
pub fn new_concurrent_hashmap(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized("java/util/concurrent/ConcurrentHashMap")
        .unwrap();
    let chm = ctx.alloc_object(cid, 4);
    call(
        reg,
        ctx,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();
    chm
}

/// Allocate a fresh `java/util/LinkedHashMap`. `access_order = true` builds
/// it via the `(IFZ)V` ctor with capacity 16, load factor 0.75.
pub fn new_linked_hashmap(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    access_order: bool,
) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized("java/util/LinkedHashMap")
        .unwrap();
    let lhm = ctx.alloc_object(cid, 8);
    if access_order {
        call(
            reg,
            ctx,
            "java/util/LinkedHashMap",
            "<init>",
            "(IFZ)V",
            &[
                Value::Object(Some(lhm)),
                Value::Int(16),
                Value::Float(0.75),
                Value::Int(1),
            ],
        )
        .unwrap();
    } else {
        call(
            reg,
            ctx,
            "java/util/LinkedHashMap",
            "<init>",
            "()V",
            &[Value::Object(Some(lhm))],
        )
        .unwrap();
    }
    lhm
}

/// Allocate a fresh empty `java/util/TreeMap`.
pub fn new_treemap(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> ObjectRef {
    let cid = ctx.ensure_class_initialized("java/util/TreeMap").unwrap();
    let tm = ctx.alloc_object(cid, 4);
    call(
        reg,
        ctx,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();
    tm
}

/// Declare `java.lang.Comparable` on a mocked wrapper class, as the real one
/// declares it.
///
/// `MockCtx::ensure_class_initialized` mints a bare name — no interfaces, and
/// `superclass_of` is `None` — so `implements_comparable` answers *false* for
/// every mocked object unless a test says otherwise. That is fine while the
/// production code only ever ORDERS these values: `natural_compare` unboxes a
/// primitive wrapper and never asks about the interface.
///
/// It stopped being fine when `tree_natural_order_key_check` landed. That guard
/// models the JDK's `addEntryToEmptyMap` self-compare — `TreeMap.put` on an
/// EMPTY natural-order map does `compare(key, key)`, which is a `checkcast
/// java/lang/Comparable` — and it asks `implements_comparable` directly, not
/// `natural_compare`. So the first `put` of a mocked `Integer` began throwing
/// `ClassCastException: class java.lang.Integer cannot be cast to class
/// java.lang.Comparable`, and `treemap_for_each_reads_forwarded_action_and_pairs`
/// went red without ever reaching the GC-pin behaviour it exists to assert.
///
/// The production code is right — `probes/TreeNaturalKeyProbe` diffs 14 rows of
/// TreeMap/TreeSet natural-order behaviour against HotSpot JDK 25 and is
/// byte-identical in both `--real-jdk` and `--jdk-only`, including the CCE for a
/// genuinely non-Comparable key. It was the MOCK that did not model
/// `java.lang.Integer`.
fn declare_comparable(ctx: &mut MockCtx, cid: cratonvm_types::ClassId) {
    let comparable = ctx
        .ensure_class_initialized("java/lang/Comparable")
        .expect("mock class minting is infallible");
    ctx.set_class_interfaces(cid, vec![comparable]);
}

/// Box an i32 as a synthetic `java.lang.Integer` so the keys behave as
/// objects in the put/get dispatch path. Field layout: slot 0 = Int(v).
///
/// Uses `alloc_object` + `set_field`, which the trait exposes — the heap
/// entry's class_id carries the Integer name so `unbox_wrapper` in the
/// production code can route through `obj_to_display_string`'s wrapper
/// branch. `Comparable` is declared for the reason `declare_comparable` gives.
pub fn boxed_int(ctx: &mut MockCtx, v: i32) -> Value {
    let cid = ctx.ensure_class_initialized("java/lang/Integer").unwrap();
    declare_comparable(ctx, cid);
    let obj = ctx.alloc_object(cid, 1);
    ctx.set_field(obj, 0, Value::Int(v));
    Value::Object(Some(obj))
}

/// Box a `char` as a synthetic `java.lang.Character` (slot 0 = Int code unit).
pub fn boxed_char(ctx: &mut MockCtx, c: char) -> Value {
    let cid = ctx.ensure_class_initialized("java/lang/Character").unwrap();
    declare_comparable(ctx, cid);
    let obj = ctx.alloc_object(cid, 1);
    ctx.set_field(obj, 0, Value::Int(c as i32));
    Value::Object(Some(obj))
}

/// Return the class name (e.g. `"java/lang/Character"`) of a heap object.
pub fn class_name_of(ctx: &MockCtx, obj: ObjectRef) -> Option<String> {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
}
