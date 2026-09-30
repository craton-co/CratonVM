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
    /// gc-common w13-e: set by [`MockNativeContext::script_channel`], so the
    /// mock serves `read(Ljava/nio/ByteBuffer;)I` from `stream_bytes` into
    /// the buffer's `hb` array, like a `ReadableByteChannel` over a fresh
    /// heap buffer. Off by default.
    channel_scripted: bool,
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
    /// How many upcoming `try_new_array` calls REFUSE (answer `None`), the
    /// way the VM's fallible allocator does on a heap with no hole that big.
    /// Zero by default, so the fallible allocator behaves exactly as the
    /// trait default (`Some(new_array(..))`) and no existing test changes.
    ///
    /// gc-common w10-b: before this the crate had no refusal-capable mock,
    /// so the reclaim-and-retry ladder of the two `readAllBytes` doors
    /// (`new_byte_array_reclaiming`, `common-w8b-native-factories-still-
    /// single-attempt` item 6) could not be exercised at all.
    try_array_refusals: usize,
    /// `try_new_array` calls seen, refused or not.
    try_array_calls: usize,
    /// What `reclaim_before_alloc_retry` answers. `false` by default, which
    /// is the trait default ("no reclamation attempted").
    reclaim_succeeds: bool,
    /// `reclaim_before_alloc_retry` calls seen.
    reclaim_calls: usize,
    /// How many `try_new_array` calls to let through before the armed
    /// refusals start (gc-common w12-e): a native that allocates a small
    /// array first (`readUTF`'s two-byte length) can then be refused at the
    /// caller-sized one. Zero by default.
    try_array_refuse_skip: usize,
    /// gc-common w12-e: when set, the mock keeps a real native pin stack and
    /// every `reclaim_before_alloc_retry` MOVES EVERY OBJECT to a fresh
    /// address, rewriting the references the heap, the by-name fields, the
    /// global roots and the pins hold -- what a moving collection does. A
    /// native that keeps using a Rust local across the reclaim instead of
    /// re-reading it through its pin then hits `entry_index`'s
    /// "invalid ObjectRef" panic. Off by default: every other test keeps the
    /// trait-default pins (handle 0, `read_native_pin` returns the fallback).
    reclaim_relocates: bool,
    /// The pin stack while `reclaim_relocates` is set.
    pins: Vec<ObjectRef>,
    /// Relocations performed so far.
    relocations: usize,
    /// gc-common w16-c: scripted answers for STATIC `invoke` calls, matched
    /// by `(method, descriptor)` and consumed, like `scripts` is for
    /// `invoke_virtual`. Empty by default, so `invoke` keeps answering
    /// `Ok(None)` for every existing test.
    invoke_scripts: Vec<InvokeScript>,
    /// gc-common w20-f: a one-shot `(method, descriptor)` whose next
    /// `invoke_virtual` MOVES EVERY OBJECT before it answers -- the moving
    /// collection a Java call can run. `None` by default. See
    /// [`MockNativeContext::w20f_relocate_on_invoke`].
    w20f_relocating_invoke: Option<(String, String)>,
    /// gc-common w21-b: a one-shot class name whose next `new_object` MOVES
    /// EVERY OBJECT before it allocates -- the collection an allocation can
    /// run. `None` by default. See
    /// [`MockNativeContext::w21b_relocate_on_new_object`].
    w21b_relocating_new_object: Option<String>,
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
            channel_scripted: false,
            global_roots: HashMap::new(),
            next_gref: 1,
            buffer_field_aliasing: false,
            try_array_refusals: 0,
            try_array_calls: 0,
            reclaim_succeeds: false,
            reclaim_calls: 0,
            try_array_refuse_skip: 0,
            reclaim_relocates: false,
            pins: Vec::new(),
            relocations: 0,
            invoke_scripts: Vec::new(),
            w20f_relocating_invoke: None,
            w21b_relocating_new_object: None,
        }
    }

    /// Make the next `new_object(class)` a moving collection (gc-common
    /// w21-b): every object moves, as [`MockNativeContext::relocate_on_reclaim`]
    /// does at a reclaim, before the new object is allocated. Turns on the
    /// real pin stack that relocation needs.
    pub(crate) fn w21b_relocate_on_new_object(&mut self, class: &str) {
        self.reclaim_relocates = true;
        self.w21b_relocating_new_object = Some(class.to_string());
    }

    /// Make the next `invoke_virtual(_, method, desc, _)` a moving collection
    /// (gc-common w20-f): every object moves, exactly as
    /// [`MockNativeContext::relocate_on_reclaim`] does at a reclaim, before the
    /// call answers. Turns on the real pin stack that relocation needs.
    pub(crate) fn w20f_relocate_on_invoke(&mut self, method: &str, desc: &str) {
        self.reclaim_relocates = true;
        self.w20f_relocating_invoke = Some((method.to_string(), desc.to_string()));
    }

    /// Answer the next static `invoke(_, method, desc, _)` with `result`
    /// (gc-common w16-c). See [`MockNativeContext::invoke_scripts`].
    pub(crate) fn script_invoke(&mut self, method: &str, desc: &str, result: MethodCallResult) {
        self.invoke_scripts.push(InvokeScript {
            method_name: method.to_string(),
            descriptor: desc.to_string(),
            result,
        });
    }

    /// Global roots currently held (`add_global_root` minus
    /// `remove_global_root`).
    pub(crate) fn global_root_count(&self) -> usize {
        self.global_roots.len()
    }

    /// A mock whose objects are handed out from `base` upward.
    ///
    /// Every [`MockNativeContext::new`] starts at address 8, so two mocks in
    /// one test -- or two parallel tests -- mint the SAME addresses. Two live
    /// VMs never do, and the process-wide identity-keyed native tables (the
    /// `nio` rows among them) match rows on the address: two "VMs" built with
    /// `new()` would find each other's rows. A test that needs two VMs, or a
    /// row no parallel test can touch, builds them here with distinct bases.
    /// `base` must be non-zero and 8-byte aligned, and below `i32::MAX` so
    /// the mock's identity hash (the address) stays distinct too.
    pub(crate) fn with_address_base(base: usize) -> Self {
        assert!(
            base != 0 && base % 8 == 0 && base < i32::MAX as usize,
            "with_address_base: {base:#x} must be non-zero, 8-aligned and below i32::MAX"
        );
        Self {
            next_ptr: base,
            ..Self::new()
        }
    }

    /// Make the next `n` `try_new_array` calls refuse. See
    /// [`MockNativeContext::try_array_refusals`].
    pub(crate) fn refuse_next_try_arrays(&mut self, n: usize) {
        self.try_array_refusals = n;
    }

    /// Choose what `reclaim_before_alloc_retry` answers.
    pub(crate) fn set_reclaim_succeeds(&mut self, succeeds: bool) {
        self.reclaim_succeeds = succeeds;
    }

    /// `(try_new_array calls, reclaim_before_alloc_retry calls)` so far.
    pub(crate) fn alloc_ladder_counts(&self) -> (usize, usize) {
        (self.try_array_calls, self.reclaim_calls)
    }

    /// Let the next `skip` `try_new_array` calls succeed, then refuse `n`.
    /// See [`MockNativeContext::try_array_refuse_skip`].
    pub(crate) fn refuse_try_arrays_after(&mut self, skip: usize, n: usize) {
        self.try_array_refuse_skip = skip;
        self.try_array_refusals = n;
    }

    /// Make every reclaim a moving collection. See
    /// [`MockNativeContext::reclaim_relocates`].
    pub(crate) fn relocate_on_reclaim(&mut self) {
        self.reclaim_relocates = true;
    }

    /// Relocations performed by reclaims so far.
    pub(crate) fn relocation_count(&self) -> usize {
        self.relocations
    }

    /// Pins still held (only meaningful with `relocate_on_reclaim`).
    pub(crate) fn pinned_count(&self) -> usize {
        self.pins.len()
    }

    /// Move every object to a fresh address and rewrite every reference the
    /// mock can see: object fields, array elements, by-name fields (keys and
    /// values), the string and class side tables, global roots and pins. The
    /// old addresses are forgotten, so a stale `ObjectRef` panics on its next
    /// use.
    ///
    /// gc-common w29-c: a move is now what the VM's moving collection is to
    /// the crate's weak side tables. (1) It PRESERVES the identity hash, as the
    /// real heap does: the mock's hash is the address truncated to `i32`, so
    /// every object moves by the same multiple of 4 GiB (a fresh band per
    /// relocation, above every address `alloc_entry` hands out). (2) It then
    /// runs the stop-the-world epilogue's native-io sweeps
    /// (`gc_sweep_io_side_tables`, `zip_real_jar::gc_sweep_jar_rows`) with the
    /// move's pointer map and nothing judged dead, so a row filed before the
    /// move is re-filed under its owner's new address -- the side tables are
    /// keyed by the owner's CURRENT address since w29-c, and without this a
    /// native that re-reads its receiver through a pin after the move would
    /// miss its own row, which no real collection can make it do.
    fn relocate_every_object(&mut self) {
        fn remap(value: &mut Value, moved: &HashMap<usize, usize>) {
            if let Value::Object(Some(obj)) = value {
                if let Some(&to) = moved.get(&(obj.as_ptr() as usize)) {
                    // SAFETY: `to` is a fresh, non-null, 8-aligned mock address
                    // minted just below and entered in `ptr_to_index`.
                    *obj = unsafe { ObjectRef::from_raw(to as *mut u8) };
                }
            }
        }
        let old: Vec<(usize, usize)> = self.ptr_map_ref().iter().map(|(&p, &i)| (p, i)).collect();
        let mut moved: HashMap<usize, usize> = HashMap::with_capacity(old.len());
        let mut new_map: HashMap<usize, usize> = HashMap::with_capacity(old.len());
        // Hash-preserving (see above): one 4 GiB band per relocation. Adding
        // the same offset to every (distinct) address keeps them distinct, and
        // the result is never below 4 GiB, where `alloc_entry` allocates.
        let offset = (self.relocations + 1) << 32;
        for (ptr, idx) in old {
            let to = ptr + offset;
            moved.insert(ptr, to);
            new_map.insert(to, idx);
        }
        *self.ptr_map_mut() = new_map;
        for entry in self.heap_mut().iter_mut() {
            match entry {
                HeapEntry::Object { fields } => fields.iter_mut().for_each(|v| remap(v, &moved)),
                HeapEntry::Array { elements, .. } => {
                    elements.iter_mut().for_each(|v| remap(v, &moved))
                }
            }
        }
        let named = std::mem::take(self.named_fields_mut());
        for ((ptr, name), mut value) in named {
            remap(&mut value, &moved);
            let ptr = moved.get(&ptr).copied().unwrap_or(ptr);
            self.named_fields_mut().insert((ptr, name), value);
        }
        let strings = std::mem::take(self.strings_mut());
        for (ptr, text) in strings {
            let ptr = moved.get(&ptr).copied().unwrap_or(ptr);
            self.strings_mut().insert(ptr, text);
        }
        self.obj_class = std::mem::take(&mut self.obj_class)
            .into_iter()
            .map(|(ptr, class)| (moved.get(&ptr).copied().unwrap_or(ptr), class))
            .collect();
        for root in self.global_roots.values_mut() {
            if let Some(&to) = moved.get(&(root.as_ptr() as usize)) {
                // SAFETY: as in `remap`.
                *root = unsafe { ObjectRef::from_raw(to as *mut u8) };
            }
        }
        for pin in self.pins.iter_mut() {
            if let Some(&to) = moved.get(&(pin.as_ptr() as usize)) {
                // SAFETY: as in `remap`.
                *pin = unsafe { ObjectRef::from_raw(to as *mut u8) };
            }
        }
        self.relocations += 1;
        // The epilogue's native-io sweeps (gc-common w29-c): re-file every
        // moved owner's rows; nothing is judged dead here.
        let pointer_map: cratonvm_types::PointerMap = moved.into_iter().collect();
        crate::gc_sweep_io_side_tables(self.vm_identity, &pointer_map, &|_| true);
        crate::zip_real_jar::gc_sweep_jar_rows(self.vm_identity, &pointer_map, &|_| true);
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

    /// Make `invoke_virtual(_, "read", "(Ljava/nio/ByteBuffer;)I", ..)` behave
    /// like a `ReadableByteChannel` over `bytes`: it fills the buffer's `hb`
    /// array from index 0 (a fresh heap buffer's position) with as many bytes
    /// as fit and returns the count, then `-1` at exhaustion.
    pub(crate) fn script_channel(&mut self, bytes: &[u8]) {
        // SAFETY: as in `script_input_stream`.
        let q = unsafe { &mut *self.stream_bytes.get() };
        q.clear();
        q.extend(bytes.iter().copied());
        self.channel_scripted = true;
    }

    /// Serve one `read(ByteBuffer)` from the scripted channel.
    fn serve_channel_read(&mut self, args: &[Value]) -> MethodCallResult {
        let bb = match args.first() {
            Some(Value::Object(Some(b))) => *b,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let arr = match self.get_field_by_name(bb, "hb") {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let room = self.array_length(arr);
        let chunk: Vec<u8> = {
            // SAFETY: the mock is single-threaded and `&mut self` excludes
            // concurrent or aliased access to the scripted queue.
            let q = unsafe { &mut *self.stream_bytes.get() };
            let n = room.min(q.len());
            q.drain(..n).collect()
        };
        if chunk.is_empty() {
            return Ok(Some(Value::Int(-1)));
        }
        for (i, b) in chunk.iter().enumerate() {
            self.set_array_element(arr, i, Value::Int(*b as i8 as i32));
        }
        Ok(Some(Value::Int(chunk.len() as i32)))
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
        // gc-common w20-f: the scripted moving collection inside this call.
        let relocate_here = matches!(
            &self.w20f_relocating_invoke,
            Some((m, d)) if m == method_name && d == descriptor
        );
        if relocate_here {
            self.w20f_relocating_invoke = None;
            self.relocate_every_object();
        }
        // Scripted InputStream takes precedence over the static script table.
        if self.stream_scripted && method_name == "read" && descriptor == "([BII)I" {
            return self.serve_stream_read(args);
        }
        if self.channel_scripted
            && method_name == "read"
            && descriptor == "(Ljava/nio/ByteBuffer;)I"
        {
            return self.serve_channel_read(args);
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
        if let Some(pos) = self
            .invoke_scripts
            .iter()
            .position(|s| s.method_name == m && s.descriptor == d)
        {
            return self.invoke_scripts.remove(pos).result;
        }
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
        // gc-common w21-b: the scripted moving collection inside this
        // allocation.
        if self.w21b_relocating_new_object.as_deref() == Some(class) {
            self.w21b_relocating_new_object = None;
            self.relocate_every_object();
        }
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
    /// The trait default (`Some(new_array(..))`) unless a test armed
    /// [`MockNativeContext::refuse_next_try_arrays`].
    fn try_new_array(&mut self, et: ArrayElementType, length: usize) -> Option<ObjectRef> {
        self.try_array_calls += 1;
        if self.try_array_refuse_skip > 0 {
            self.try_array_refuse_skip -= 1;
            return Some(self.new_array(et, length));
        }
        if self.try_array_refusals > 0 {
            self.try_array_refusals -= 1;
            return None;
        }
        Some(self.new_array(et, length))
    }
    /// Counts the call and answers [`MockNativeContext::set_reclaim_succeeds`]
    /// (`false` by default, the trait default). With
    /// [`MockNativeContext::relocate_on_reclaim`] it also moves every object,
    /// whatever it answers: a declined reclaim may still have collected.
    fn reclaim_before_alloc_retry(&mut self) -> bool {
        self.reclaim_calls += 1;
        if self.reclaim_relocates {
            self.relocate_every_object();
        }
        self.reclaim_succeeds
    }
    /// The trait default (handle 0) unless
    /// [`MockNativeContext::relocate_on_reclaim`] is set; then a real stack.
    fn pin_native_root(&mut self, obj: ObjectRef) -> usize {
        if !self.reclaim_relocates {
            return 0;
        }
        self.pins.push(obj);
        self.pins.len() - 1
    }
    fn read_native_pin(&self, handle: usize, fallback: ObjectRef) -> ObjectRef {
        if !self.reclaim_relocates {
            return fallback;
        }
        // gc-common w21: a read past the end is a pin an older pin's release
        // already dropped (`unpin_native_roots` truncates), see `common-w20v`.
        assert!(
            handle == usize::MAX || handle < self.pins.len(),
            "read_native_pin({handle}) on a pin stack of {} -- the pin was released",
            self.pins.len()
        );
        self.pins.get(handle).copied().unwrap_or(fallback)
    }
    fn unpin_native_roots(&mut self, base: usize) {
        if self.reclaim_relocates && base < self.pins.len() {
            self.pins.truncate(base);
        }
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
    fn capture_stack_trace(&mut self) -> Vec<StackTraceEntry> {
        Vec::new()
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
// Cross-module test serialization for the two path-policy globals:
// `set_path_confine_to_cwd` AND `set_path_validation_enabled`.
//
// `PATH_CONFINE_TO_CWD` is a global atomic; tests that flip it briefly
// would otherwise race with parallel tests that depend on it being off
// (e.g. the WatchService tests in `watch.rs` register absolute /tmp
// paths). A single process-wide mutex guarantees only one confinement
// test runs at a time, and the test always pairs `set(true)` with
// `set(false)` while still holding the guard.
//
// `PATH_VALIDATION_ENABLED` (2026-09-11) is the same species and was not
// covered here, which is a stronger defect than it sounds: a test that turns
// validation OFF turns off the `..`-traversal guard for the WHOLE PROCESS,
// so any parallel test asserting that a traversal path is REJECTED can read
// it accepted instead. MEASURED: `files_validated_path_rejects_dotdot_segment`
// failed about one run in twelve of `cargo test -p cratonvm-native-io --lib`
// with `Files path with `..` segment accepted: Ok("../../etc/passwd")`, and
// that is a security assertion silently not being made. Three tests turned the
// flag off without this guard (`path_validation_disabled_allows_dotdot`,
// `path_validation_disabled_still_rejects_null_byte`,
// `files_validated_path_rejects_null_byte_even_when_disabled`) and three more
// read a verdict that depends on it being on. All six now take it. A test that
// touches either global, or whose assertion depends on either, must hold this.
// ---------------------------------------------------------------------------

use parking_lot::Mutex as PlMutex;
use std::sync::OnceLock as StdOnceLock;

pub(crate) fn confine_test_lock() -> &'static PlMutex<()> {
    static LOCK: StdOnceLock<PlMutex<()>> = StdOnceLock::new();
    LOCK.get_or_init(|| PlMutex::new(()))
}

/// gc-common w10-b: the reclaim-and-retry ladder of the two `readAllBytes`
/// doors (`crate::new_byte_array_reclaiming`, added by w9-d for
/// `common-w8b-native-factories-still-single-attempt` item 6), exercised
/// through the refusal knobs above. The ladder is: one `try_new_array`; on a
/// refusal one `reclaim_before_alloc_retry`; if that reclaimed, exactly one
/// more `try_new_array`; otherwise a catchable `OutOfMemoryError`.
mod read_all_bytes_reclaim_ladder_tests {
    use super::*;
    use cratonvm_types::error::{RuntimeError, VmError};

    fn is_oome(r: &MethodCallResult) -> bool {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError { .. }
            )))
        )
    }

    fn bytes_of(ctx: &MockNativeContext, r: &MethodCallResult) -> Vec<u8> {
        let arr = match r {
            Ok(Some(Value::Object(Some(a)))) => *a,
            other => panic!("expected a byte[], got {other:?}"),
        };
        (0..ctx.array_length(arr))
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => v as u8,
                other => panic!("byte[] element {i} is {other:?}"),
            })
            .collect()
    }

    /// `InputStream.readAllBytes` over a scripted stream.
    fn is_read_all(ctx: &mut MockNativeContext, payload: &[u8]) -> MethodCallResult {
        ctx.script_input_stream(payload);
        let this = ctx.alloc_object(0);
        crate::native_is_read_all_bytes(ctx, &[Value::Object(Some(this))])
    }

    #[test]
    fn input_stream_read_all_bytes_reclaims_then_retries() {
        let mut ctx = MockNativeContext::new();
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = is_read_all(&mut ctx, b"reclaimed payload");
        assert_eq!(bytes_of(&ctx, &r), b"reclaimed payload");
        assert_eq!(
            ctx.alloc_ladder_counts(),
            (2, 1),
            "one refused try, one reclaim, one successful retry"
        );
    }

    #[test]
    fn input_stream_read_all_bytes_first_try_needs_no_reclaim() {
        let mut ctx = MockNativeContext::new();
        let r = is_read_all(&mut ctx, b"abc");
        assert_eq!(bytes_of(&ctx, &r), b"abc");
        assert_eq!(ctx.alloc_ladder_counts(), (1, 0));
    }

    #[test]
    fn input_stream_read_all_bytes_declined_reclaim_is_oome_without_retry() {
        let mut ctx = MockNativeContext::new();
        ctx.refuse_next_try_arrays(1);
        let r = is_read_all(&mut ctx, b"abc");
        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(
            ctx.alloc_ladder_counts(),
            (1, 1),
            "a declined reclaim must not retry"
        );
    }

    #[test]
    fn input_stream_read_all_bytes_retries_exactly_once() {
        let mut ctx = MockNativeContext::new();
        ctx.refuse_next_try_arrays(2);
        ctx.set_reclaim_succeeds(true);
        let r = is_read_all(&mut ctx, b"abc");
        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(
            ctx.alloc_ladder_counts(),
            (2, 1),
            "the ladder is one reclaim and one retry, never a loop"
        );
    }

    /// Unique temp file holding `data`; removed by the caller.
    fn temp_file(tag: &str, data: &[u8]) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("cratonvm_w10b_{tag}_{nanos}.bin"));
        std::fs::write(&p, data).expect("temp file");
        p
    }

    /// `Files.readAllBytes(path)`, with the path in the slot `read_path_str`
    /// falls back to (the shape `files_bulk_transfer_tests` uses).
    fn files_read_all(ctx: &mut MockNativeContext, path: &std::path::Path) -> MethodCallResult {
        // The string first, then the object that holds it: nothing allocates
        // between binding `obj` and storing into it (the shape
        // `scripts/stale-handle-across-alloc-audit.py` screens for).
        let s = ctx.create_string(&path.to_string_lossy());
        let obj = ctx.alloc_object(1);
        ctx.set_field(obj, crate::PATH_FIELD_STR, Value::Object(Some(s)));
        crate::native_files_read_all_bytes(ctx, &[Value::Object(Some(obj))])
    }

    #[test]
    fn files_read_all_bytes_reclaims_then_retries() {
        let _g = confine_test_lock().lock();
        let prev = crate::is_path_confine_to_cwd();
        crate::set_path_confine_to_cwd(false);
        let data = [0x00u8, 0x7f, 0x80, 0xff, b'w', b'1', b'0'];
        let path = temp_file("reclaim", &data);

        let mut ctx = MockNativeContext::new();
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = files_read_all(&mut ctx, &path);
        let _ = std::fs::remove_file(&path);
        crate::set_path_confine_to_cwd(prev);

        assert_eq!(bytes_of(&ctx, &r), data);
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1));
    }

    #[test]
    fn files_read_all_bytes_declined_reclaim_is_oome_without_retry() {
        let _g = confine_test_lock().lock();
        let prev = crate::is_path_confine_to_cwd();
        crate::set_path_confine_to_cwd(false);
        let path = temp_file("declined", b"abc");

        let mut ctx = MockNativeContext::new();
        ctx.refuse_next_try_arrays(1);
        let r = files_read_all(&mut ctx, &path);
        let _ = std::fs::remove_file(&path);
        crate::set_path_confine_to_cwd(prev);

        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(ctx.alloc_ladder_counts(), (1, 1));
    }
}

/// gc-common w12-e (`common-w8b-native-factories-still-single-attempt`
/// residue 3): the caller-sized reads and read-outs that now reclaim and
/// retry before their `OutOfMemoryError`. Each runs on a mock whose reclaim
/// MOVES every object ([`MockNativeContext::relocate_on_reclaim`]), so a
/// native that kept a Rust local across the reclaim instead of re-reading it
/// through a pin panics with "invalid ObjectRef" instead of passing.
mod w12e_caller_sized_read_reclaim_tests {
    use super::*;
    use cratonvm_types::error::{RuntimeError, VmError};

    fn is_oome(r: &MethodCallResult) -> bool {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError { .. }
            )))
        )
    }

    fn array_of(r: &MethodCallResult) -> ObjectRef {
        match r {
            Ok(Some(Value::Object(Some(a)))) => *a,
            other => panic!("expected an object, got {other:?}"),
        }
    }

    fn bytes_in(ctx: &MockNativeContext, arr: ObjectRef) -> Vec<u8> {
        (0..ctx.array_length(arr))
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => v as u8,
                other => panic!("byte[] element {i} is {other:?}"),
            })
            .collect()
    }

    fn chars_in(ctx: &MockNativeContext, arr: ObjectRef, n: usize) -> String {
        let units: Vec<u16> = (0..n)
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => v as u16,
                other => panic!("char[] element {i} is {other:?}"),
            })
            .collect();
        String::from_utf16(&units).expect("valid UTF-16")
    }

    /// A `ByteArrayOutputStream` in the synthetic layout, holding `payload`
    /// in a backing array with spare capacity.
    fn baos_holding(ctx: &mut MockNativeContext, payload: &[u8]) -> ObjectRef {
        let data = ctx.new_array(ArrayElementType::Byte, payload.len() + 7);
        assert!(ctx.write_byte_array_from(data, 0, payload));
        let this = ctx.alloc_object(2);
        ctx.set_field(this, crate::BAOS_FIELD_DATA, Value::Object(Some(data)));
        ctx.set_field(this, crate::BAOS_FIELD_COUNT, Value::Int(payload.len() as i32));
        this
    }

    #[test]
    fn baos_to_byte_array_reclaims_and_copies_from_the_moved_buffer() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let this = baos_holding(&mut ctx, b"w12e toByteArray");
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_baos_to_byte_array(&mut ctx, &[Value::Object(Some(this))]);
        let arr = array_of(&r);
        assert_eq!(bytes_in(&ctx, arr), b"w12e toByteArray");
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1), "refused, reclaimed, retried");
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0, "the backing-array pin is released");
    }

    #[test]
    fn baos_to_byte_array_declined_reclaim_is_oome_and_releases_its_pin() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let this = baos_holding(&mut ctx, b"abc");
        ctx.refuse_next_try_arrays(1);
        let r = crate::native_baos_to_byte_array(&mut ctx, &[Value::Object(Some(this))]);
        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(ctx.alloc_ladder_counts(), (1, 1), "a declined reclaim must not retry");
        assert_eq!(ctx.pinned_count(), 0, "the error path releases the pin");
    }

    #[test]
    fn baos_to_byte_array_first_try_copies_exactly_count_bytes() {
        let mut ctx = MockNativeContext::new();
        let this = baos_holding(&mut ctx, b"xyz");
        let r = crate::native_baos_to_byte_array(&mut ctx, &[Value::Object(Some(this))]);
        assert_eq!(bytes_in(&ctx, array_of(&r)), b"xyz");
        assert_eq!(ctx.alloc_ladder_counts(), (1, 0));
    }

    #[test]
    fn caw_to_char_array_reclaims_and_copies_from_the_moved_buffer() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let text = "w12e toCharArray \u{263a}";
        let units: Vec<u16> = text.encode_utf16().collect();
        let buf = ctx.new_array(ArrayElementType::Char, units.len() + 5);
        for (i, u) in units.iter().enumerate() {
            ctx.set_array_element(buf, i, Value::Int(*u as i32));
        }
        let this = ctx.alloc_object(2);
        ctx.set_field(this, crate::CAW_FIELD_BUF, Value::Object(Some(buf)));
        ctx.set_field(this, crate::CAW_FIELD_COUNT, Value::Int(units.len() as i32));
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_caw_to_char_array(&mut ctx, &[Value::Object(Some(this))]);
        let arr = array_of(&r);
        assert_eq!(ctx.array_length(arr), units.len());
        assert_eq!(chars_in(&ctx, arr, units.len()), text);
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1));
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
    }

    /// A `DataInputStream` over a stream scripted with `text` in `readUTF`'s
    /// wire format (a two-byte big-endian length, then the bytes).
    fn dis_holding(ctx: &mut MockNativeContext, text: &str) -> ObjectRef {
        let mut wire = (text.len() as u16).to_be_bytes().to_vec();
        wire.extend_from_slice(text.as_bytes());
        ctx.script_input_stream(&wire);
        let inner = ctx.alloc_object(0);
        let this = ctx.alloc_object(1);
        ctx.set_field(this, crate::DIS_FIELD_IN, Value::Object(Some(inner)));
        this
    }

    /// `readUTF`: the two-byte length's scratch array is let through, then
    /// the payload's caller-sized scratch is refused.
    #[test]
    fn dis_read_utf_payload_scratch_reclaims_with_the_stream_pinned() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let text = "w12e readUTF payload";
        let this = dis_holding(&mut ctx, text);
        ctx.refuse_try_arrays_after(1, 1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_dis_read_utf(&mut ctx, &[Value::Object(Some(this))]);
        let s = ctx.read_string(array_of(&r)).expect("a String");
        assert_eq!(s, text);
        assert_eq!(
            ctx.alloc_ladder_counts(),
            (3, 1),
            "length scratch, refused payload scratch, one retry"
        );
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
    }

    #[test]
    fn dis_read_utf_declined_reclaim_is_oome_and_releases_the_stream_pin() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let this = dis_holding(&mut ctx, "abc");
        ctx.refuse_try_arrays_after(1, 1);
        let r = crate::native_dis_read_utf(&mut ctx, &[Value::Object(Some(this))]);
        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1));
        assert_eq!(ctx.pinned_count(), 0);
    }

    /// `InputStreamReader.read(char[],int,int)` for `text.len()` chars into a
    /// fresh `char[out_len]`. The carry-over table is a process-wide map keyed
    /// by `(vm_identity, identity hash, owner address)` (gc-common w15-c /
    /// w29-c; every mock is its own VM), and the VM's rows are removed here
    /// (the reader may have moved, so its row is no longer at `this`'s
    /// address). Returns the result and a global root naming the output array.
    fn isr_read(ctx: &mut MockNativeContext, text: &[u8], out_len: usize) -> (MethodCallResult, usize) {
        ctx.script_input_stream(text);
        let in_stream = ctx.alloc_object(0);
        let this = ctx.alloc_object(2);
        ctx.set_field(this, 1, Value::Object(Some(in_stream)));
        let out = ctx.new_array(ArrayElementType::Char, out_len);
        let out_root = ctx.add_global_root(out);
        let r = crate::native_isr_read_chars(
            ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(out)),
                Value::Int(0),
                Value::Int(text.len() as i32),
            ],
        );
        crate::forget_vm_io_side_tables(ctx.vm_identity());
        (r, out_root)
    }

    #[test]
    fn isr_read_chars_scratch_reclaims_and_decodes_into_the_moved_array() {
        let mut ctx = MockNativeContext::with_address_base(0x5E12_0000);
        ctx.relocate_on_reclaim();
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let (r, out_root) = isr_read(&mut ctx, b"hello", 8);
        assert!(matches!(r, Ok(Some(Value::Int(5)))), "got {r:?}");
        let out = ctx.resolve_global_root(out_root).expect("rooted");
        assert_eq!(chars_in(&ctx, out, 5), "hello");
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1));
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
    }

    #[test]
    fn isr_read_chars_declined_reclaim_is_oome_and_releases_its_pins() {
        let mut ctx = MockNativeContext::with_address_base(0x5E13_0000);
        ctx.relocate_on_reclaim();
        ctx.refuse_next_try_arrays(1);
        let (r, _) = isr_read(&mut ctx, b"hello", 8);
        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(ctx.alloc_ladder_counts(), (1, 1));
        assert_eq!(ctx.pinned_count(), 0);
    }
}

/// gc-common w13-e (`common-w12e-native-io-write-side-growth-single-attempt`):
/// the write-side growth (`ByteArrayOutputStream`, `StringWriter`,
/// `CharArrayWriter`), the caller-sized constructors
/// (`ByteArrayOutputStream(int)`, `StringWriter(int)`,
/// `BufferedOutputStream(out, size)`) and native-io's `ByteBuffer.allocate`
/// now reclaim and retry before their `OutOfMemoryError`. Each reclaim here is
/// a MOVING collection ([`MockNativeContext::relocate_on_reclaim`]), so a
/// native that kept a Rust local across it instead of re-reading it through a
/// pin panics with "invalid ObjectRef" rather than passing. Objects a test
/// inspects afterwards are named through global roots, which the mock remaps.
mod w13e_write_side_growth_reclaim_tests {
    use super::*;
    use cratonvm_types::error::{RuntimeError, VmError};

    const BAOS: &str = "java/io/ByteArrayOutputStream";

    fn runtime_error(r: &MethodCallResult) -> Option<&RuntimeError> {
        match r {
            Err(MethodCallFailed::InternalError(VmError::Runtime(e))) => Some(e),
            _ => None,
        }
    }

    fn is_oome(r: &MethodCallResult) -> bool {
        matches!(runtime_error(r), Some(RuntimeError::OutOfMemoryError { .. }))
    }

    fn bytes_in(ctx: &MockNativeContext, arr: ObjectRef, n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => v as u8,
                other => panic!("byte[] element {i} is {other:?}"),
            })
            .collect()
    }

    fn chars_array(ctx: &mut MockNativeContext, text: &str) -> ObjectRef {
        let units: Vec<u16> = text.encode_utf16().collect();
        let arr = ctx.new_array(ArrayElementType::Char, units.len());
        for (i, u) in units.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*u as i32));
        }
        arr
    }

    fn chars_in(ctx: &MockNativeContext, arr: ObjectRef, n: usize) -> String {
        let units: Vec<u16> = (0..n)
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => v as u16,
                other => panic!("char[] element {i} is {other:?}"),
            })
            .collect();
        String::from_utf16(&units).expect("valid UTF-16")
    }

    fn rooted(ctx: &MockNativeContext, root: usize) -> ObjectRef {
        ctx.resolve_global_root(root).expect("rooted")
    }

    fn field_obj(ctx: &MockNativeContext, obj: ObjectRef, slot: usize) -> ObjectRef {
        match ctx.get_field(obj, slot) {
            Value::Object(Some(o)) => o,
            other => panic!("slot {slot} is {other:?}"),
        }
    }

    fn string_of(ctx: &MockNativeContext, r: MethodCallResult) -> String {
        match r {
            Ok(Some(Value::Object(Some(o)))) => ctx.read_string(o).expect("a String"),
            other => panic!("expected a String, got {other:?}"),
        }
    }

    /// A full `ByteArrayOutputStream` (count == capacity) holding `payload`,
    /// rooted so the test can find it after a relocation.
    fn full_baos(ctx: &mut MockNativeContext, payload: &[u8]) -> (ObjectRef, usize) {
        let data = ctx.new_array(ArrayElementType::Byte, payload.len());
        assert!(ctx.write_byte_array_from(data, 0, payload));
        let this = ctx.alloc_object_with_class(2, BAOS);
        ctx.set_field(this, crate::BAOS_FIELD_DATA, Value::Object(Some(data)));
        ctx.set_field(this, crate::BAOS_FIELD_COUNT, Value::Int(payload.len() as i32));
        let root = ctx.add_global_root(this);
        (this, root)
    }

    fn baos_contents(ctx: &MockNativeContext, root: usize) -> Vec<u8> {
        let this = rooted(ctx, root);
        let count = match ctx.get_field(this, crate::BAOS_FIELD_COUNT) {
            Value::Int(v) => v as usize,
            other => panic!("count is {other:?}"),
        };
        bytes_in(ctx, field_obj(ctx, this, crate::BAOS_FIELD_DATA), count)
    }

    #[test]
    fn grown_capacity_doubles_and_clamps_like_arrays_support() {
        let soft = crate::RECLAIM_MAX_ARRAY_LENGTH;
        assert_eq!(crate::grown_array_capacity(4, 5), 8);
        assert_eq!(crate::grown_array_capacity(4, 100), 100);
        assert_eq!(crate::grown_array_capacity(0, 1), 1);
        // Doubling past SOFT_MAX asks for SOFT_MAX, not 2x.
        assert_eq!(crate::grown_array_capacity(soft / 2 + 10, soft / 2 + 11), soft);
        // A requirement past SOFT_MAX is asked for as is (and then refused).
        assert_eq!(crate::grown_array_capacity(soft, soft + 5), soft + 5);
    }

    /// `write(byte[],int,int)` on a full stream: the growth is refused, the
    /// reclaim moves every object (the stream, its old `data` AND the source
    /// array), and the retry must copy from the moved source.
    #[test]
    fn baos_write_bytes_growth_reclaims_with_the_source_pinned() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let (this, root) = full_baos(&mut ctx, b"abcd");
        let src = ctx.new_array(ArrayElementType::Byte, 8);
        assert!(ctx.write_byte_array_from(src, 0, b"efghijkl"));
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_baos_write_bytes(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(src)),
                Value::Int(0),
                Value::Int(8),
            ],
        );
        assert!(matches!(r, Ok(None)), "got {r:?}");
        assert_eq!(baos_contents(&ctx, root), b"abcdefghijkl");
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1), "refused, reclaimed, retried");
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0, "every pin is released");
    }

    #[test]
    fn baos_write_bytes_declined_reclaim_is_oome_and_leaves_the_stream_intact() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let (this, root) = full_baos(&mut ctx, b"abcd");
        let src = ctx.new_array(ArrayElementType::Byte, 2);
        ctx.refuse_next_try_arrays(1);
        let r = crate::native_baos_write_bytes(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(src)),
                Value::Int(0),
                Value::Int(2),
            ],
        );
        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(baos_contents(&ctx, root), b"abcd", "count and data unchanged");
        assert_eq!(ctx.alloc_ladder_counts(), (1, 1), "a declined reclaim must not retry");
        assert_eq!(ctx.pinned_count(), 0, "the error path releases its pins");
    }

    #[test]
    fn baos_write_byte_growth_reclaims_and_appends_to_the_moved_stream() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let (this, root) = full_baos(&mut ctx, b"xy");
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_baos_write(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Int(b'z' as i32)],
        );
        assert!(matches!(r, Ok(None)), "got {r:?}");
        assert_eq!(baos_contents(&ctx, root), b"xyz");
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1));
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
    }

    /// No growth needed: one bulk copy, no allocation at all.
    #[test]
    fn baos_write_bytes_within_capacity_allocates_nothing() {
        let mut ctx = MockNativeContext::new();
        let data = ctx.new_array(ArrayElementType::Byte, 8);
        let this = ctx.alloc_object_with_class(2, BAOS);
        ctx.set_field(this, crate::BAOS_FIELD_DATA, Value::Object(Some(data)));
        ctx.set_field(this, crate::BAOS_FIELD_COUNT, Value::Int(0));
        let root = ctx.add_global_root(this);
        let src = ctx.new_array(ArrayElementType::Byte, 5);
        assert!(ctx.write_byte_array_from(src, 0, b"01234"));
        let r = crate::native_baos_write_bytes(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(src)),
                Value::Int(1),
                Value::Int(3),
            ],
        );
        assert!(matches!(r, Ok(None)), "got {r:?}");
        assert_eq!(baos_contents(&ctx, root), b"123");
        assert_eq!(ctx.alloc_ladder_counts(), (0, 0));
    }

    #[test]
    fn baos_int_constructor_reclaims_with_the_receiver_pinned() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let this = ctx.alloc_object_with_class(2, BAOS);
        let root = ctx.add_global_root(this);
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_baos_init_capacity(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Int(100)],
        );
        assert!(matches!(r, Ok(None)), "got {r:?}");
        let this = rooted(&ctx, root);
        assert_eq!(ctx.array_length(field_obj(&ctx, this, crate::BAOS_FIELD_DATA)), 100);
        assert!(matches!(ctx.get_field(this, crate::BAOS_FIELD_COUNT), Value::Int(0)));
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1));
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
    }

    #[test]
    fn baos_int_constructor_refuses_a_negative_size_before_allocating() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let this = ctx.alloc_object_with_class(2, BAOS);
        let r = crate::native_baos_init_capacity(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Int(-1)],
        );
        assert!(
            matches!(runtime_error(&r), Some(RuntimeError::IllegalArgumentException { .. })),
            "got {r:?}"
        );
        assert_eq!(ctx.alloc_ladder_counts(), (0, 0));
        assert_eq!(ctx.pinned_count(), 0);
    }

    /// `new StringWriter(2)`, then `write(char[],0,5)` whose growth is refused
    /// once: the source array moves with everything else.
    #[test]
    fn string_writer_growth_reclaims_with_the_source_pinned() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let this = ctx.alloc_object(2);
        let root = ctx.add_global_root(this);
        let r = crate::native_sw_init_cap(&mut ctx, &[Value::Object(Some(this)), Value::Int(2)]);
        assert!(matches!(r, Ok(None)), "got {r:?}");
        assert_eq!(ctx.pinned_count(), 0);
        let this = rooted(&ctx, root);
        let src = chars_array(&mut ctx, "hello");
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_sw_write_chars(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(src)),
                Value::Int(0),
                Value::Int(5),
            ],
        );
        assert!(matches!(r, Ok(None)), "got {r:?}");
        let this = rooted(&ctx, root);
        let r = crate::native_sw_to_string(&mut ctx, &[Value::Object(Some(this))]);
        assert_eq!(string_of(&ctx, r), "hello");
        assert_eq!(ctx.alloc_ladder_counts(), (3, 1), "constructor, refused growth, retry");
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
    }

    /// `append(char)` is `write(int)` then `return this`: the receiver it
    /// returns must be the post-reclaim address, not `args[0]`.
    #[test]
    fn string_writer_append_returns_the_moved_receiver() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let this = ctx.alloc_object(2);
        let root = ctx.add_global_root(this);
        let r = crate::native_sw_init_cap(&mut ctx, &[Value::Object(Some(this)), Value::Int(1)]);
        assert!(matches!(r, Ok(None)), "got {r:?}");
        let this = rooted(&ctx, root);
        let r = crate::native_sw_write_int(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Int('a' as i32)],
        );
        assert!(matches!(r, Ok(None)), "got {r:?}");
        let this = rooted(&ctx, root);
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_sw_append_char(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Int('b' as i32)],
        );
        let returned = match r {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("append gave {other:?}"),
        };
        assert_eq!(ctx.relocation_count(), 1);
        let moved = rooted(&ctx, root);
        assert_eq!(returned.as_ptr(), moved.as_ptr(), "append returns the moved receiver");
        assert_ne!(returned.as_ptr(), this.as_ptr());
        let r = crate::native_sw_to_string(&mut ctx, &[Value::Object(Some(moved))]);
        assert_eq!(string_of(&ctx, r), "ab");
        assert_eq!(ctx.pinned_count(), 0);
    }

    #[test]
    fn string_writer_rejects_a_negative_size_and_a_bad_range() {
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(2);
        let r = crate::native_sw_init_cap(&mut ctx, &[Value::Object(Some(this)), Value::Int(-1)]);
        assert!(
            matches!(runtime_error(&r), Some(RuntimeError::IllegalArgumentException { .. })),
            "got {r:?}"
        );
        assert_eq!(ctx.alloc_ladder_counts(), (0, 0), "refused before allocating");
        let r = crate::native_sw_init_cap(&mut ctx, &[Value::Object(Some(this)), Value::Int(4)]);
        assert!(matches!(r, Ok(None)), "got {r:?}");
        let src = chars_array(&mut ctx, "abc");
        let r = crate::native_sw_write_chars(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(src)),
                Value::Int(0),
                Value::Int(-1),
            ],
        );
        assert!(
            matches!(runtime_error(&r), Some(RuntimeError::IndexOutOfBoundsException { .. })),
            "got {r:?}"
        );
        assert_eq!(ctx.alloc_ladder_counts(), (1, 0), "the bad range allocates nothing");
    }

    /// A `CharArrayWriter` whose buffer is exactly full with `text`, rooted.
    fn full_caw(ctx: &mut MockNativeContext, text: &str) -> (ObjectRef, usize) {
        let buf = chars_array(ctx, text);
        let this = ctx.alloc_object(2);
        ctx.set_field(this, crate::CAW_FIELD_BUF, Value::Object(Some(buf)));
        let count = text.encode_utf16().count() as i32;
        ctx.set_field(this, crate::CAW_FIELD_COUNT, Value::Int(count));
        let root = ctx.add_global_root(this);
        (this, root)
    }

    fn caw_contents(ctx: &MockNativeContext, root: usize) -> String {
        let this = rooted(ctx, root);
        let count = match ctx.get_field(this, crate::CAW_FIELD_COUNT) {
            Value::Int(v) => v as usize,
            other => panic!("count is {other:?}"),
        };
        chars_in(ctx, field_obj(ctx, this, crate::CAW_FIELD_BUF), count)
    }

    #[test]
    fn char_array_writer_growth_reclaims_with_the_source_pinned() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let (this, root) = full_caw(&mut ctx, "abcd");
        let src = chars_array(&mut ctx, "..efg\u{263a}");
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_caw_write_bulk(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(src)),
                Value::Int(2),
                Value::Int(4),
            ],
        );
        assert!(matches!(r, Ok(None)), "got {r:?}");
        assert_eq!(caw_contents(&ctx, root), "abcdefg\u{263a}");
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1));
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
    }

    #[test]
    fn char_array_writer_declined_reclaim_is_oome_and_leaves_the_writer_intact() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let (this, root) = full_caw(&mut ctx, "abcd");
        ctx.refuse_next_try_arrays(1);
        let r = crate::native_caw_write(&mut ctx, &[Value::Object(Some(this)), Value::Int('e' as i32)]);
        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(caw_contents(&ctx, root), "abcd");
        assert_eq!(ctx.alloc_ladder_counts(), (1, 1));
        assert_eq!(ctx.pinned_count(), 0);
    }

    #[test]
    fn char_array_writer_rejects_a_bad_range_before_growing() {
        let mut ctx = MockNativeContext::new();
        let (this, root) = full_caw(&mut ctx, "abcd");
        let src = chars_array(&mut ctx, "xy");
        let r = crate::native_caw_write_bulk(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(src)),
                Value::Int(1),
                Value::Int(2),
            ],
        );
        assert!(
            matches!(runtime_error(&r), Some(RuntimeError::IndexOutOfBoundsException { .. })),
            "got {r:?}"
        );
        assert_eq!(caw_contents(&ctx, root), "abcd");
        assert_eq!(ctx.alloc_ladder_counts(), (0, 0));
    }

    /// `new BufferedOutputStream(out, 64)`: `out` used to be held as a bare
    /// `Value` across the allocation and stored afterwards.
    #[test]
    fn buffered_output_stream_sized_constructor_reclaims_with_out_pinned() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let this = ctx.alloc_object(3);
        let out = ctx.alloc_object(0);
        let this_root = ctx.add_global_root(this);
        let out_root = ctx.add_global_root(out);
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_bos_init_size(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(out)), Value::Int(64)],
        );
        assert!(matches!(r, Ok(None)), "got {r:?}");
        let moved = rooted(&ctx, this_root);
        let moved_out = rooted(&ctx, out_root);
        assert_eq!(field_obj(&ctx, moved, crate::BOS_FIELD_OUT).as_ptr(), moved_out.as_ptr());
        assert_eq!(ctx.array_length(field_obj(&ctx, moved, crate::BOS_FIELD_BUF)), 64);
        assert!(matches!(ctx.get_field(moved, crate::BOS_FIELD_COUNT), Value::Int(0)));
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1));
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
    }

    #[test]
    fn buffered_output_stream_declined_reclaim_is_oome_and_releases_its_pins() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let this = ctx.alloc_object(3);
        let out = ctx.alloc_object(0);
        ctx.refuse_next_try_arrays(1);
        let r = crate::native_bos_init_size(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(out)), Value::Int(64)],
        );
        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(ctx.alloc_ladder_counts(), (1, 1));
        assert_eq!(ctx.pinned_count(), 0);
    }

    /// A receiver with no `buf`/`count` slots keeps its bytes in the side
    /// buffer; the `byte[size]` it used to allocate and drop is gone. The side
    /// table is keyed by `(vm_identity, identity hash)` since gc-common w14-e,
    /// and every mock is its own VM; the address base is kept from w13-e, when
    /// the key was the bare hash. The test removes its row.
    #[test]
    fn buffered_output_stream_side_buffer_layout_allocates_nothing() {
        let mut ctx = MockNativeContext::with_address_base(0x5E14_0000);
        let this = ctx.alloc_object(1);
        let out = ctx.alloc_object(0);
        let key = crate::io_side_row_key(&ctx, this);
        let r = crate::native_bos_init_size(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(out)), Value::Int(1 << 20)],
        );
        let side = crate::bos_side_buffers().lock().remove(&key).map(|s| s.capacity);
        assert!(matches!(r, Ok(None)), "got {r:?}");
        assert_eq!(side, Some(1 << 20));
        assert_eq!(field_obj(&ctx, this, crate::BOS_FIELD_OUT).as_ptr(), out.as_ptr());
        assert_eq!(ctx.alloc_ladder_counts(), (0, 0));
    }

    #[test]
    fn byte_buffer_allocate_reclaims_with_the_new_buffer_pinned() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = crate::native_bb_allocate(&mut ctx, &[Value::Int(16)]);
        let bb = match r {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("allocate gave {other:?}"),
        };
        let hb = match ctx.get_field_by_name(bb, "hb") {
            Value::Object(Some(a)) => a,
            other => panic!("hb is {other:?}"),
        };
        assert_eq!(ctx.array_length(hb), 16);
        assert_eq!(field_obj(&ctx, bb, crate::BB_FIELD_ARRAY).as_ptr(), hb.as_ptr());
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1));
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
    }

    #[test]
    fn byte_buffer_allocate_declined_reclaim_is_oome_and_releases_its_pin() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        ctx.refuse_next_try_arrays(1);
        let r = crate::native_bb_allocate(&mut ctx, &[Value::Int(16)]);
        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(ctx.alloc_ladder_counts(), (1, 1));
        assert_eq!(ctx.pinned_count(), 0);
    }
}

/// gc-common w14-e (`common-w13e-bos-side-buffers-process-wide-keyed-by-identity-hash`):
/// the `BufferedOutputStream` / `PipedOutputStream` side buffers. A receiver
/// with one field has `out` at slot 0 and no `buf`/`count` slots (the mock
/// resolves no names, so `bos_slots` answers the synthetic 0/1/2), which is
/// the side-buffer path. Every mock is its own VM (`vm_identity`), so these
/// tests share no row with each other or with any parallel test.
mod w14e_bos_side_buffer_tests {
    use super::*;
    use cratonvm_types::error::{RuntimeError, VmError};

    /// A side-buffered stream of `capacity`, wrapping a fresh `out`.
    fn side_stream(ctx: &mut MockNativeContext, capacity: i32) -> ObjectRef {
        let this = ctx.alloc_object(1);
        let out = ctx.alloc_object(0);
        let r = crate::native_bos_init_size(
            ctx,
            &[Value::Object(Some(this)), Value::Object(Some(out)), Value::Int(capacity)],
        );
        assert!(matches!(r, Ok(None)), "init gave {r:?}");
        this
    }

    fn write(ctx: &mut MockNativeContext, this: ObjectRef, b: u8) {
        let r = crate::native_bos_write(ctx, &[Value::Object(Some(this)), Value::Int(b as i32)]);
        assert!(matches!(r, Ok(None)), "write gave {r:?}");
    }

    fn flush(ctx: &mut MockNativeContext, this: ObjectRef) -> MethodCallResult {
        crate::native_bos_flush(ctx, &[Value::Object(Some(this))])
    }

    /// `(bytes, capacity, owner)` of `key`'s row.
    fn row(key: crate::IoRowKey) -> Option<(Vec<u8>, usize, usize)> {
        crate::bos_side_buffers()
            .lock()
            .get(&key)
            .map(|r| (r.bytes.clone(), r.capacity, r.owner))
    }

    /// The byte chunks handed to `out.write(byte[], int, int)`, in order.
    fn written_chunks(ctx: &MockNativeContext) -> Vec<Vec<u8>> {
        ctx.recorded_calls()
            .iter()
            .filter(|c| c.method_name == "write" && c.descriptor == "([BII)V")
            .map(|c| {
                let arr = match c.args.first() {
                    Some(Value::Object(Some(a))) => *a,
                    other => panic!("write's array is {other:?}"),
                };
                let len = match c.args.get(2) {
                    Some(Value::Int(n)) => *n as usize,
                    other => panic!("write's length is {other:?}"),
                };
                (0..len)
                    .map(|i| match ctx.get_array_element(arr, i) {
                        Value::Int(v) => v as u8,
                        other => panic!("byte[] element {i} is {other:?}"),
                    })
                    .collect()
            })
            .collect()
    }

    fn is_oome(r: &MethodCallResult) -> bool {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError { .. }
            )))
        )
    }

    /// The retire criterion's first half: two VMs whose streams have EQUAL
    /// identity hashes. The row used to be keyed by the bare hash, so both
    /// bytes landed in one row and VM A's flush delivered "ab".
    #[test]
    fn two_vms_with_equal_identity_hashes_keep_their_own_side_buffers() {
        let mut a = MockNativeContext::with_address_base(0x5E17_0000);
        let mut b = MockNativeContext::with_address_base(0x5E17_0000);
        let sa = side_stream(&mut a, 4);
        let sb = side_stream(&mut b, 4);
        assert_eq!(a.identity_hash_code(sa), b.identity_hash_code(sb));
        assert_ne!(a.vm_identity(), b.vm_identity());
        write(&mut a, sa, b'a');
        write(&mut b, sb, b'b');
        let (key_a, key_b) = (crate::io_side_row_key(&a, sa), crate::io_side_row_key(&b, sb));
        assert_ne!(key_a, key_b);
        assert_eq!(row(key_a).map(|r| r.0), Some(b"a".to_vec()));
        assert_eq!(row(key_b).map(|r| r.0), Some(b"b".to_vec()));
        assert!(matches!(flush(&mut a, sa), Ok(None)));
        assert_eq!(written_chunks(&a), vec![b"a".to_vec()]);
        assert_eq!(row(key_b).map(|r| r.0), Some(b"b".to_vec()), "VM B's bytes stay in VM B");
        crate::forget_vm_io_side_tables(a.vm_identity());
        crate::forget_vm_io_side_tables(b.vm_identity());
    }

    /// The retire criterion's second half: the post-collection sweep. A row
    /// whose stream moved follows it; a row whose stream died is dropped with
    /// its bytes; another VM's row is never judged.
    #[test]
    fn the_sweep_follows_a_moved_stream_and_drops_a_dead_one() {
        let mut ctx = MockNativeContext::new();
        let mut other = MockNativeContext::new();
        let moved = side_stream(&mut ctx, 16);
        let dead = side_stream(&mut ctx, 16);
        let foreign = side_stream(&mut other, 16);
        write(&mut ctx, moved, 1);
        write(&mut ctx, dead, 2);
        write(&mut other, foreign, 3);
        let moved_key = crate::io_side_row_key(&ctx, moved);
        let dead_key = crate::io_side_row_key(&ctx, dead);
        let foreign_key = crate::io_side_row_key(&other, foreign);
        assert_eq!(row(moved_key).map(|r| r.2), Some(moved.as_ptr() as usize));

        // A real move keeps the identity hash (the mock's is the low 32 bits
        // of the address), and the row is re-filed under the new address.
        let to = moved.as_ptr() as usize + (1usize << 32);
        let to_key = crate::io_row_key(crate::io_side_key(&ctx, moved), to);
        let mut map = cratonvm_types::PointerMap::new();
        map.insert(moved.as_ptr() as usize, to);
        let dropped = crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map, &|_| false);
        assert_eq!(dropped, 1);
        assert_eq!(row(to_key), Some((vec![1], 16, to)), "the moved stream keeps its row");
        assert_eq!(row(moved_key), None, "re-filed, not copied");
        assert_eq!(row(dead_key), None, "the dead stream's row and bytes are gone");
        assert_eq!(row(foreign_key).map(|r| r.0), Some(vec![3]), "another VM's row is untouched");

        // A live verdict on the re-addressed owner keeps it, with no map.
        let empty = cratonvm_types::PointerMap::new();
        assert_eq!(crate::gc_sweep_io_side_tables(ctx.vm_identity(), &empty, &|a| a == to), 0);
        assert_eq!(row(to_key).map(|r| r.2), Some(to));

        crate::forget_vm_io_side_tables(ctx.vm_identity());
        crate::forget_vm_io_side_tables(other.vm_identity());
    }

    #[test]
    fn forgetting_a_vm_drops_only_its_rows() {
        let mut a = MockNativeContext::new();
        let mut b = MockNativeContext::new();
        let sa = side_stream(&mut a, 8);
        let sb = side_stream(&mut b, 8);
        let (key_a, key_b) = (crate::io_side_row_key(&a, sa), crate::io_side_row_key(&b, sb));
        assert_eq!(crate::forget_vm_io_side_tables(a.vm_identity()), 1);
        assert_eq!(row(key_a), None);
        assert!(row(key_b).is_some());
        assert_eq!(crate::forget_vm_io_side_tables(a.vm_identity()), 0, "idempotent");
        crate::forget_vm_io_side_tables(b.vm_identity());
    }

    /// A write that reaches a stream no native constructor saw creates the
    /// row with the default capacity and names its stream as the owner.
    #[test]
    fn a_lazily_created_row_names_its_stream() {
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(1);
        let out = ctx.alloc_object(0);
        ctx.set_field(this, crate::BOS_FIELD_OUT, Value::Object(Some(out)));
        write(&mut ctx, this, 7);
        let key = crate::io_side_row_key(&ctx, this);
        assert_eq!(
            row(key),
            Some((vec![7], crate::BOS_SIDE_DEFAULT_CAPACITY, this.as_ptr() as usize))
        );
        crate::forget_vm_io_side_tables(ctx.vm_identity());
    }

    /// `new BufferedOutputStream(out, Integer.MAX_VALUE)` on the side path
    /// used to `Vec::with_capacity` the whole request, which aborts the
    /// process when the host allocator refuses it.
    #[test]
    fn a_huge_requested_capacity_is_not_reserved_up_front() {
        let mut ctx = MockNativeContext::new();
        let this = side_stream(&mut ctx, i32::MAX);
        let key = crate::io_side_row_key(&ctx, this);
        {
            let bufs = crate::bos_side_buffers().lock();
            let r = bufs.get(&key).expect("row");
            assert_eq!(r.capacity, i32::MAX as usize);
            assert!(r.bytes.capacity() <= crate::BOS_SIDE_DEFAULT_CAPACITY);
        }
        crate::forget_vm_io_side_tables(ctx.vm_identity());
    }

    /// `PipedOutputStream.close()` was the bare flush, so no pipe's row was
    /// ever removed.
    #[test]
    fn closing_a_pipe_flushes_and_drops_its_row() {
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(1);
        let sink = ctx.alloc_object(0);
        let r = crate::native_pos_init_connected(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(sink))],
        );
        assert!(matches!(r, Ok(None)), "init gave {r:?}");
        write(&mut ctx, this, b'p');
        let key = crate::io_side_row_key(&ctx, this);
        assert!(row(key).is_some());
        let r = crate::native_pos_close(&mut ctx, &[Value::Object(Some(this))]);
        assert!(matches!(r, Ok(None)), "close gave {r:?}");
        assert_eq!(written_chunks(&ctx), vec![b"p".to_vec()]);
        assert_eq!(row(key), None);
        crate::forget_vm_io_side_tables(ctx.vm_identity());
    }

    /// `flushBuffer()` resets `count` only after `out.write` returns: bytes a
    /// failed write took are still buffered, and the next flush delivers them
    /// first. They used to be dropped.
    #[test]
    fn a_failed_side_flush_keeps_its_bytes() {
        let mut ctx = MockNativeContext::new();
        let this = side_stream(&mut ctx, 16);
        let key = crate::io_side_row_key(&ctx, this);
        write(&mut ctx, this, b'x');
        write(&mut ctx, this, b'y');
        ctx.script(
            "write",
            "([BII)V",
            Err(RuntimeError::IOException {
                message: "sink refused".into(),
            }
            .into()),
        );
        assert!(flush(&mut ctx, this).is_err());
        assert_eq!(row(key).map(|r| r.0), Some(b"xy".to_vec()));
        write(&mut ctx, this, b'z');
        assert!(matches!(flush(&mut ctx, this), Ok(None)));
        assert_eq!(written_chunks(&ctx).last(), Some(&b"xyz".to_vec()));
        assert_eq!(row(key).map(|r| r.0), Some(Vec::new()));
        crate::forget_vm_io_side_tables(ctx.vm_identity());
    }

    /// The flush's `byte[]` reclaims and retries before its OOME, and the
    /// reclaim here MOVES every object: the flush must re-read the stream (and
    /// the `out` it wraps) through its pin, or the mock panics on the stale
    /// address. It used to call the infallible `new_array` with `out` held
    /// bare across it and across `write`.
    #[test]
    fn side_flush_reclaims_and_rereads_the_stream_after_a_moving_collection() {
        let mut ctx = MockNativeContext::new();
        ctx.relocate_on_reclaim();
        let this = side_stream(&mut ctx, 16);
        // Followed through the move by a global root (the mock rewrites them),
        // because the row now lives at the stream's NEW address (gc-common
        // w29-c: the mock's move re-files it, as the VM's epilogue does).
        let this_root = ctx.add_global_root(this);
        for b in [1u8, 2, 3] {
            write(&mut ctx, this, b);
        }
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let r = flush(&mut ctx, this);
        assert!(matches!(r, Ok(None)), "flush gave {r:?}");
        assert_eq!(ctx.alloc_ladder_counts(), (2, 1));
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
        assert_eq!(written_chunks(&ctx), vec![vec![1, 2, 3]]);
        assert!(ctx
            .recorded_calls()
            .iter()
            .any(|c| c.method_name == "flush" && c.descriptor == "()V"));
        let moved = ctx.resolve_global_root(this_root).expect("rooted");
        assert_ne!(moved.as_ptr(), this.as_ptr(), "the stream moved");
        assert_eq!(row(crate::io_side_row_key(&ctx, moved)).map(|r| r.0), Some(Vec::new()));
        assert_eq!(row(crate::io_side_row_key(&ctx, this)), None, "nothing left at the old address");
        crate::forget_vm_io_side_tables(ctx.vm_identity());
    }

    #[test]
    fn a_side_flush_oome_keeps_its_bytes() {
        let mut ctx = MockNativeContext::new();
        let this = side_stream(&mut ctx, 16);
        let key = crate::io_side_row_key(&ctx, this);
        write(&mut ctx, this, 9);
        ctx.refuse_next_try_arrays(1);
        let r = flush(&mut ctx, this);
        assert!(is_oome(&r), "expected OutOfMemoryError, got {r:?}");
        assert_eq!(ctx.alloc_ladder_counts(), (1, 1));
        assert_eq!(row(key).map(|r| r.0), Some(vec![9]));
        assert!(written_chunks(&ctx).is_empty());
        crate::forget_vm_io_side_tables(ctx.vm_identity());
    }
}

/// gc-common w15-c (`common-w14e-reader-and-watch-side-tables-keyed-by-bare-identity-hash`):
/// the `InputStreamReader` carry-over (`ISR_PENDING`), `BufferedReader` fill
/// buffers (`br_buf_table`), `StringReader` content (`SR_STATE`) and
/// `WatchService` watchers (`WATCH_SERVICES`) are keyed by
/// `(vm_identity, identity hash)`, name their owner, and are swept by
/// `gc_sweep_io_side_tables` / forgotten by `forget_vm_io_side_tables`.
///
/// Every mock is its own VM (`vm_identity`), so no parallel test can share a
/// row; each test forgets its own VMs' rows at the end. The two-VM tests build
/// both mocks at ONE address base so their objects have EQUAL identity hashes,
/// which is the collision the old bare-hash key could not tell apart.
mod w15c_reader_watch_side_table_tests {
    use super::*;
    use cratonvm_types::error::{RuntimeError, VmError};

    fn forget(ctxs: &[&MockNativeContext]) {
        for ctx in ctxs {
            crate::forget_vm_io_side_tables(ctx.vm_identity());
        }
    }

    fn is_runtime(r: &MethodCallResult, want: fn(&RuntimeError) -> bool) -> bool {
        matches!(r, Err(MethodCallFailed::InternalError(VmError::Runtime(e))) if want(e))
    }

    fn is_stream_closed(r: &MethodCallResult) -> bool {
        is_runtime(r, |e| {
            matches!(e, RuntimeError::IOException { message } if message == "Stream closed")
        })
    }

    fn map_one(from: ObjectRef, to: usize) -> cratonvm_types::PointerMap {
        let mut map = cratonvm_types::PointerMap::new();
        map.insert(from.as_ptr() as usize, to);
        map
    }

    /// Where a real (hash-preserving) move of `obj` lands: the mock's identity
    /// hash is the low 32 bits of the address, so the move adds 4 GiB
    /// (gc-common w29-c; the rows are keyed by the owner's current address).
    fn moved_to(obj: ObjectRef) -> usize {
        obj.as_ptr() as usize + (1usize << 32)
    }

    /// The row key of `obj` after the sweep re-filed it at `to`.
    fn key_at(ctx: &MockNativeContext, obj: ObjectRef, to: usize) -> crate::IoRowKey {
        crate::io_row_key(crate::io_side_key(ctx, obj), to)
    }

    /// A reference to the moved object, for natives that only hash and
    /// compare it (never read its fields: the mock heap does not know `to`).
    fn at(to: usize) -> ObjectRef {
        // SAFETY: never dereferenced; 8-aligned because every mock address is.
        unsafe { ObjectRef::from_raw(to as *mut u8) }
    }

    // ---------------------------------------------------------------- ISR

    /// An `InputStreamReader` stand-in: the wrapped stream at slot 1.
    fn isr_reader(ctx: &mut MockNativeContext) -> ObjectRef {
        let in_stream = ctx.alloc_object(0);
        let this = ctx.alloc_object(2);
        ctx.set_field(this, 1, Value::Object(Some(in_stream)));
        this
    }

    /// `read(char[], 0, len)` over `bytes`; the result and the `char[]`.
    fn isr_read(
        ctx: &mut MockNativeContext,
        this: ObjectRef,
        bytes: &[u8],
        len: usize,
    ) -> (MethodCallResult, ObjectRef) {
        ctx.script_input_stream(bytes);
        let out = ctx.new_array(ArrayElementType::Char, len);
        let r = crate::native_isr_read_chars(
            ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(out)),
                Value::Int(0),
                Value::Int(len as i32),
            ],
        );
        (r, out)
    }

    /// `(pending bytes, owner)` of `key`'s carry-over row.
    fn isr_row(key: crate::IoRowKey) -> Option<(Vec<u8>, usize)> {
        crate::isr_pending()
            .lock()
            .get(&key)
            .map(|r| (r.pending.clone(), r.owner))
    }

    /// The retire criterion for `ISR_PENDING`: VM A leaves half of a UTF-8
    /// sequence pending, then VM B's reader (equal identity hash) reads. The
    /// bare-hash key handed B A's pending byte (B decoded U+FFFD instead of
    /// 'b'), and A lost the first half of its character.
    #[test]
    fn isr_two_vms_with_equal_identity_hashes_keep_their_own_carry_over() {
        let mut a = MockNativeContext::with_address_base(0x5E1C_0000);
        let mut b = MockNativeContext::with_address_base(0x5E1C_0000);
        let ra = isr_reader(&mut a);
        let rb = isr_reader(&mut b);
        assert_eq!(a.identity_hash_code(ra), b.identity_hash_code(rb));
        let (key_a, key_b) = (crate::io_side_row_key(&a, ra), crate::io_side_row_key(&b, rb));
        assert_ne!(key_a, key_b);

        // 0xC3 0xA9 is 'é'. A reads only its first byte.
        let (r, _) = isr_read(&mut a, ra, &[0xC3], 1);
        assert!(matches!(r, Ok(Some(Value::Int(0)))), "A's first read gave {r:?}");
        assert_eq!(isr_row(key_a).map(|r| r.0), Some(vec![0xC3]));

        let (r, out) = isr_read(&mut b, rb, b"b", 1);
        assert!(matches!(r, Ok(Some(Value::Int(1)))), "B's read gave {r:?}");
        assert_eq!(b.get_array_element(out, 0), Value::Int(b'b' as i32));
        assert_eq!(isr_row(key_a).map(|r| r.0), Some(vec![0xC3]), "A's carry-over stays A's");

        let (r, out) = isr_read(&mut a, ra, &[0xA9], 1);
        assert!(matches!(r, Ok(Some(Value::Int(1)))), "A's second read gave {r:?}");
        assert_eq!(a.get_array_element(out, 0), Value::Int(0xE9));
        forget(&[&a, &b]);
    }

    /// The sweep: a moved reader's row follows it, a dead reader's goes,
    /// another VM's is never judged.
    #[test]
    fn isr_sweep_follows_a_moved_reader_and_drops_a_dead_one() {
        let mut ctx = MockNativeContext::new();
        let mut other = MockNativeContext::new();
        let moved = isr_reader(&mut ctx);
        let dead = isr_reader(&mut ctx);
        let foreign = isr_reader(&mut other);
        let _ = isr_read(&mut ctx, moved, &[0xE2], 1);
        let _ = isr_read(&mut ctx, dead, &[0xE2], 1);
        let _ = isr_read(&mut other, foreign, &[0xE2], 1);
        let moved_key = crate::io_side_row_key(&ctx, moved);
        let dead_key = crate::io_side_row_key(&ctx, dead);
        let foreign_key = crate::io_side_row_key(&other, foreign);
        assert_eq!(isr_row(moved_key).map(|r| r.1), Some(moved.as_ptr() as usize));

        let to = moved_to(moved);
        let dropped =
            crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map_one(moved, to), &|_| false);
        assert_eq!(dropped, 1);
        assert_eq!(
            isr_row(key_at(&ctx, moved, to)),
            Some((vec![0xE2], to)),
            "the moved reader keeps its row, re-filed at its new address"
        );
        assert_eq!(isr_row(moved_key), None, "nothing left at the old address");
        assert_eq!(isr_row(dead_key), None, "the dead reader's row is gone");
        assert!(isr_row(foreign_key).is_some(), "another VM's row is untouched");
        forget(&[&ctx, &other]);
    }

    /// The delegated read can move the reader. The row must name where the
    /// reader IS afterwards, or the next sweep judges a live reader dead and
    /// drops its carry-over. `this` used to be held bare across the read.
    #[test]
    fn isr_row_names_the_reader_after_a_moving_collection() {
        let mut ctx = MockNativeContext::new();
        let this = isr_reader(&mut ctx);
        let this_root = ctx.add_global_root(this);
        ctx.relocate_on_reclaim();
        ctx.refuse_next_try_arrays(1);
        ctx.set_reclaim_succeeds(true);
        let (r, _) = isr_read(&mut ctx, this, &[0xC3], 1);
        assert!(matches!(r, Ok(Some(Value::Int(0)))), "read gave {r:?}");
        assert_eq!(ctx.relocation_count(), 1);
        assert_eq!(ctx.pinned_count(), 0);
        let now = ctx.resolve_global_root(this_root).expect("rooted");
        assert_ne!(now.as_ptr(), this.as_ptr(), "the reclaim moved the reader");
        assert_eq!(
            ctx.identity_hash_code(now),
            ctx.identity_hash_code(this),
            "a real move keeps the identity hash"
        );
        assert_eq!(
            isr_row(crate::io_side_row_key(&ctx, now)),
            Some((vec![0xC3], now.as_ptr() as usize))
        );
        assert_eq!(isr_row(crate::io_side_row_key(&ctx, this)), None);
        forget(&[&ctx]);
    }

    // ----------------------------------------------------------------- BR

    /// A temp file holding `bytes`, opened in the (process-wide) mock fd
    /// table. The directory guard must outlive the fd.
    fn br_fd(ctx: &MockNativeContext, bytes: &[u8]) -> (tempfile::TempDir, u32) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("br.txt");
        std::fs::write(&path, bytes).expect("write");
        let fd = ctx
            .fd_table()
            .open_read(path.to_str().expect("utf-8 path"))
            .expect("open");
        (dir, fd)
    }

    /// A `BufferedReader` stand-in: its fd at slot 0.
    fn br_reader(ctx: &mut MockNativeContext, fd: u32) -> ObjectRef {
        let this = ctx.alloc_object(1);
        ctx.set_field(this, 0, Value::Int(fd as i32));
        this
    }

    fn br_read(ctx: &mut MockNativeContext, this: ObjectRef) -> MethodCallResult {
        crate::native_br_read(ctx, &[Value::Object(Some(this))])
    }

    fn br_owner(key: crate::IoRowKey) -> Option<usize> {
        crate::br_buf_table().lock().get(&key).map(|r| r.owner)
    }

    /// The retire criterion for `br_buf_table`: VM B's reader (equal identity
    /// hash) used to drain VM A's fill buffer, returning 'b' from A's file.
    #[test]
    fn br_two_vms_with_equal_identity_hashes_keep_their_own_buffers() {
        let mut a = MockNativeContext::with_address_base(0x5E1C_1000);
        let mut b = MockNativeContext::with_address_base(0x5E1C_1000);
        let (_da, fd_a) = br_fd(&a, b"ab");
        let (_db, fd_b) = br_fd(&b, b"xy");
        let ra = br_reader(&mut a, fd_a);
        let rb = br_reader(&mut b, fd_b);
        assert_eq!(a.identity_hash_code(ra), b.identity_hash_code(rb));

        assert!(matches!(br_read(&mut a, ra), Ok(Some(Value::Int(0x61)))));
        let r = br_read(&mut b, rb);
        assert!(matches!(r, Ok(Some(Value::Int(0x78)))), "B read {r:?}, not its own 'x'");
        assert!(matches!(br_read(&mut a, ra), Ok(Some(Value::Int(0x62)))));
        assert_eq!(br_owner(crate::io_side_row_key(&a, ra)), Some(ra.as_ptr() as usize));

        let key_a = crate::io_side_row_key(&a, ra);
        let r = crate::native_br_close(&mut a, &[Value::Object(Some(ra))]);
        assert!(matches!(r, Ok(None)), "close gave {r:?}");
        assert_eq!(br_owner(key_a), None, "close drops the row");
        let key_b = crate::io_side_row_key(&b, rb);
        assert!(br_owner(key_b).is_some(), "A's close leaves B's row");
        let r = crate::native_br_close(&mut b, &[Value::Object(Some(rb))]);
        assert!(matches!(r, Ok(None)), "close gave {r:?}");
        assert_eq!(br_owner(key_b), None);
        forget(&[&a, &b]);
    }

    #[test]
    fn br_sweep_follows_a_moved_reader_and_drops_a_dead_one() {
        let mut ctx = MockNativeContext::new();
        let mut other = MockNativeContext::new();
        let (_d1, fd1) = br_fd(&ctx, b"moved");
        let (_d2, fd2) = br_fd(&ctx, b"dead");
        let (_d3, fd3) = br_fd(&other, b"foreign");
        let moved = br_reader(&mut ctx, fd1);
        let dead = br_reader(&mut ctx, fd2);
        let foreign = br_reader(&mut other, fd3);
        assert!(matches!(br_read(&mut ctx, moved), Ok(Some(Value::Int(0x6D)))));
        assert!(matches!(br_read(&mut ctx, dead), Ok(Some(Value::Int(0x64)))));
        assert!(matches!(br_read(&mut other, foreign), Ok(Some(Value::Int(0x66)))));
        let dead_key = crate::io_side_row_key(&ctx, dead);
        let foreign_key = crate::io_side_row_key(&other, foreign);

        let to = moved_to(moved);
        let to_key = key_at(&ctx, moved, to);
        let dropped =
            crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map_one(moved, to), &|_| false);
        assert_eq!(dropped, 1);
        assert_eq!(br_owner(to_key), Some(to));
        assert_eq!(br_owner(crate::io_side_row_key(&ctx, moved)), None, "re-filed, not copied");
        assert_eq!(br_owner(dead_key), None);
        assert_eq!(br_owner(foreign_key), Some(foreign.as_ptr() as usize));
        // The moved reader's own buffer is intact at its new address: its next
        // `read()` there serves the 'o' after the 'm' (the mock heap does not
        // know `to`, so the row is read directly).
        let next = crate::br_buf_table()
            .lock()
            .get(&to_key)
            .map(|r| r.buf[r.pos]);
        assert_eq!(next, Some(b'o'));
        let _ = ctx.fd_table().close(fd1);
        let _ = ctx.fd_table().close(fd2);
        let _ = other.fd_table().close(fd3);
        forget(&[&ctx, &other]);
    }

    // ----------------------------------------------------------------- SR

    fn sr_new(ctx: &mut MockNativeContext, text: &str) -> ObjectRef {
        let this = ctx.alloc_object(1);
        let s = ctx.create_string(text);
        let r = crate::native_sr_init(ctx, &[Value::Object(Some(this)), Value::Object(Some(s))]);
        assert!(matches!(r, Ok(None)), "init gave {r:?}");
        this
    }

    fn sr_read(ctx: &mut MockNativeContext, this: ObjectRef) -> MethodCallResult {
        crate::native_sr_read(ctx, &[Value::Object(Some(this))])
    }

    fn sr_owner(key: crate::IoRowKey) -> Option<usize> {
        crate::sr_state().lock().get(&key).map(|r| r.owner)
    }

    /// The retire criterion for `SR_STATE`: VM B's `<init>` REPLACED VM A's
    /// live row, so A read B's characters, and B's `close()` closed A.
    #[test]
    fn sr_two_vms_with_equal_identity_hashes_keep_their_own_content() {
        let mut a = MockNativeContext::with_address_base(0x5E1C_2000);
        let mut b = MockNativeContext::with_address_base(0x5E1C_2000);
        let ra = sr_new(&mut a, "alpha");
        let rb = sr_new(&mut b, "beta");
        assert_eq!(a.identity_hash_code(ra), b.identity_hash_code(rb));
        assert!(matches!(sr_read(&mut a, ra), Ok(Some(Value::Int(0x61)))));
        assert!(matches!(sr_read(&mut b, rb), Ok(Some(Value::Int(0x62)))));
        let r = crate::native_sr_close(&mut b, &[Value::Object(Some(rb))]);
        assert!(matches!(r, Ok(None)));
        assert!(is_stream_closed(&sr_read(&mut b, rb)));
        assert!(
            matches!(sr_read(&mut a, ra), Ok(Some(Value::Int(0x6C)))),
            "A is still open and reads its own 'l'"
        );
        forget(&[&a, &b]);
    }

    /// A `StringReader` dropped without `close()` kept its whole input for
    /// the life of the process. The sweep drops it and follows a moved one.
    #[test]
    fn sr_sweep_follows_a_moved_reader_and_drops_a_dead_one() {
        let mut ctx = MockNativeContext::new();
        let mut other = MockNativeContext::new();
        let moved = sr_new(&mut ctx, "moved");
        let dead = sr_new(&mut ctx, "a long input nobody closed");
        let foreign = sr_new(&mut other, "foreign");
        let moved_key = crate::io_side_row_key(&ctx, moved);
        let dead_key = crate::io_side_row_key(&ctx, dead);
        let foreign_key = crate::io_side_row_key(&other, foreign);
        assert_eq!(sr_owner(moved_key), Some(moved.as_ptr() as usize));

        let to = moved_to(moved);
        let dropped =
            crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map_one(moved, to), &|_| false);
        assert_eq!(dropped, 1);
        assert_eq!(sr_owner(key_at(&ctx, moved, to)), Some(to));
        assert_eq!(sr_owner(moved_key), None, "re-filed, not copied");
        assert_eq!(sr_owner(dead_key), None);
        assert_eq!(sr_owner(foreign_key), Some(foreign.as_ptr() as usize));
        // A live verdict on the re-addressed owner keeps it.
        let empty = cratonvm_types::PointerMap::new();
        assert_eq!(crate::gc_sweep_io_side_tables(ctx.vm_identity(), &empty, &|a| a == to), 0);
        // The reader, seen where it now is, reads its own content (`read` only
        // hashes and compares the receiver, so the mock need not know `to`).
        assert!(matches!(sr_read(&mut ctx, at(to)), Ok(Some(Value::Int(0x6D)))));
        forget(&[&ctx, &other]);
    }

    /// `read(char[], off, len)` in the JDK's order: `ensureOpen()`, then
    /// `checkFromIndexSize`, then `len == 0` answers 0, then EOF answers -1.
    #[test]
    fn sr_read_chars_checks_like_the_jdk() {
        fn read(
            ctx: &mut MockNativeContext,
            this: ObjectRef,
            buf: Option<ObjectRef>,
            off: i32,
            len: i32,
        ) -> MethodCallResult {
            crate::native_sr_read_chars(
                ctx,
                &[
                    Value::Object(Some(this)),
                    Value::Object(buf),
                    Value::Int(off),
                    Value::Int(len),
                ],
            )
        }
        fn is_ioobe(r: &MethodCallResult) -> bool {
            is_runtime(r, |e| matches!(e, RuntimeError::IndexOutOfBoundsException { .. }))
        }
        let mut ctx = MockNativeContext::new();
        let this = sr_new(&mut ctx, "hey");
        let buf = ctx.new_array(ArrayElementType::Char, 4);
        let r = read(&mut ctx, this, Some(buf), -1, 2);
        assert!(is_ioobe(&r), "negative off gave {r:?}");
        let r = read(&mut ctx, this, Some(buf), 3, 2);
        assert!(is_ioobe(&r), "past-the-end range gave {r:?}");
        let r = read(&mut ctx, this, None, 0, 1);
        assert!(
            is_runtime(&r, |e| matches!(e, RuntimeError::NullPointerException { .. })),
            "null cbuf gave {r:?}"
        );
        let r = read(&mut ctx, this, Some(buf), 1, 3);
        assert!(matches!(r, Ok(Some(Value::Int(3)))), "read gave {r:?}");
        let got: Vec<Value> = (0..4).map(|i| ctx.get_array_element(buf, i)).collect();
        assert_eq!(
            got,
            vec![Value::Int(0), Value::Int(0x68), Value::Int(0x65), Value::Int(0x79)]
        );
        let r = read(&mut ctx, this, Some(buf), 0, 0);
        assert!(matches!(r, Ok(Some(Value::Int(0)))), "len 0 at EOF gave {r:?}");
        let r = read(&mut ctx, this, Some(buf), 0, 1);
        assert!(matches!(r, Ok(Some(Value::Int(-1)))), "EOF gave {r:?}");
        let _ = crate::native_sr_close(&mut ctx, &[Value::Object(Some(this))]);
        let r = read(&mut ctx, this, Some(buf), -1, 2);
        assert!(is_stream_closed(&r), "ensureOpen comes first, got {r:?}");
        forget(&[&ctx]);
    }

    /// `new StringReader(null)` is a `NullPointerException`, and files no row.
    #[test]
    fn sr_init_with_a_null_string_throws_and_files_nothing() {
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(1);
        let r = crate::native_sr_init(&mut ctx, &[Value::Object(Some(this)), Value::Object(None)]);
        assert!(
            is_runtime(&r, |e| matches!(e, RuntimeError::NullPointerException { .. })),
            "{r:?}"
        );
        assert_eq!(sr_owner(crate::io_side_row_key(&ctx, this)), None);
        forget(&[&ctx]);
    }

    // ----------------------------------------------------------------- WS

    /// A platform watcher filed for `ws`; `false` when this host refuses one
    /// (an exhausted inotify budget), and the caller then skips.
    fn ws_open(ctx: &MockNativeContext, ws: ObjectRef) -> bool {
        match crate::WatchServiceState::open(crate::io_side_owner(ws)) {
            Ok(state) => {
                crate::ws_install(ctx, ws, state);
                true
            }
            Err(e) => {
                eprintln!("skipping: the host refused a platform watcher: {e:?}");
                false
            }
        }
    }

    fn ws_owner(key: crate::IoRowKey) -> Option<usize> {
        crate::watch_services().lock().get(&key).map(|r| r.owner)
    }

    /// The retire criterion for `WATCH_SERVICES`: one VM's `close()` tore down
    /// the other VM's watcher (and its `newWatchService()` replaced it).
    #[test]
    fn ws_two_vms_with_equal_identity_hashes_keep_their_own_watchers() {
        let mut a = MockNativeContext::with_address_base(0x5E1C_3000);
        let mut b = MockNativeContext::with_address_base(0x5E1C_3000);
        let wa = a.alloc_object(0);
        let wb = b.alloc_object(0);
        assert_eq!(a.identity_hash_code(wa), b.identity_hash_code(wb));
        if !ws_open(&a, wa) || !ws_open(&b, wb) {
            forget(&[&a, &b]);
            return;
        }
        let (key_a, key_b) = (crate::io_side_row_key(&a, wa), crate::io_side_row_key(&b, wb));
        assert!(ws_owner(key_a).is_some() && ws_owner(key_b).is_some());
        let r = crate::native_ws_close(&mut b, &[Value::Object(Some(wb))]);
        assert!(matches!(r, Ok(None)), "close gave {r:?}");
        assert_eq!(ws_owner(key_b), None, "B's watcher is closed");
        assert_eq!(ws_owner(key_a), Some(wa.as_ptr() as usize), "A's watcher is untouched");
        forget(&[&a, &b]);
        assert_eq!(ws_owner(key_a), None, "teardown closes A's watcher");
    }

    /// A service dropped without `close()` kept its watcher thread and OS
    /// handles for the life of the process. The sweep closes it.
    #[test]
    fn ws_sweep_closes_a_dead_service_and_follows_a_moved_one() {
        let mut ctx = MockNativeContext::new();
        let mut other = MockNativeContext::new();
        let moved = ctx.alloc_object(0);
        let dead = ctx.alloc_object(0);
        let foreign = other.alloc_object(0);
        if !ws_open(&ctx, moved) || !ws_open(&ctx, dead) || !ws_open(&other, foreign) {
            forget(&[&ctx, &other]);
            return;
        }
        let to = moved_to(moved);
        let dropped =
            crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map_one(moved, to), &|_| false);
        assert_eq!(dropped, 1);
        assert_eq!(ws_owner(key_at(&ctx, moved, to)), Some(to));
        assert_eq!(ws_owner(crate::io_side_row_key(&ctx, moved)), None);
        assert_eq!(ws_owner(crate::io_side_row_key(&ctx, dead)), None);
        assert_eq!(
            ws_owner(crate::io_side_row_key(&other, foreign)),
            Some(foreign.as_ptr() as usize)
        );
        forget(&[&ctx, &other]);
    }

    // ------------------------------------------------------------ teardown

    /// `forget_vm_io_side_tables` covers all four tables, only for its VM, and
    /// is idempotent.
    #[test]
    fn forgetting_a_vm_drops_its_rows_in_every_reader_and_watch_table() {
        let mut ctx = MockNativeContext::new();
        let mut other = MockNativeContext::new();
        let isr = isr_reader(&mut ctx);
        let _ = isr_read(&mut ctx, isr, &[0xC3], 1);
        let (_dir, fd) = br_fd(&ctx, b"q");
        let br = br_reader(&mut ctx, fd);
        assert!(matches!(br_read(&mut ctx, br), Ok(Some(Value::Int(0x71)))));
        let _sr = sr_new(&mut ctx, "s");
        let kept = sr_new(&mut other, "kept");
        let ws = ctx.alloc_object(0);
        let watchers = usize::from(ws_open(&ctx, ws));
        assert_eq!(crate::forget_vm_io_side_tables(ctx.vm_identity()), 3 + watchers);
        assert_eq!(crate::forget_vm_io_side_tables(ctx.vm_identity()), 0, "idempotent");
        assert!(sr_owner(crate::io_side_row_key(&other, kept)).is_some(), "another VM keeps its row");
        let _ = ctx.fd_table().close(fd);
        forget(&[&other]);
    }
}

/// gc-common w16-c (`common-w15c-native-io-identity-keyed-tables-outside-the-weak-sweep`
/// rows 3 to 7): the `Scanner` input and last match (`SCAN_SOURCES`,
/// `SCAN_MATCHES`), the `DatagramChannel` fd / connected / non-blocking /
/// option rows, and the synthetic `RandomAccessFile` rows are weak on their
/// owner, swept by `gc_sweep_io_side_tables` and forgotten by
/// `forget_vm_io_side_tables`; a dead channel's or file's fd is parked on
/// `io_orphan_fds` and closed by `close_orphan_fds`; and
/// `DatagramChannel.socket()` keeps its adaptor in the channel's own `socket`
/// field instead of two global roots. Rows 1 and 2 (`StreamDecoder` /
/// `StreamEncoder`) are tested beside their private tables in
/// `stream_decoder.rs` / `stream_encoder.rs`.
///
/// Every mock is its own VM, so no parallel test can share a row; each test
/// forgets its own VMs' rows. Real sockets and files come from the mock's
/// shared fd table, whose ids are unique; each test closes what it opened.
mod w16c_io_side_table_tests {
    use super::*;

    fn forget(ctxs: &[&MockNativeContext]) {
        for ctx in ctxs {
            crate::forget_vm_io_side_tables(ctx.vm_identity());
        }
    }

    fn map_one(from: ObjectRef, to: usize) -> cratonvm_types::PointerMap {
        let mut map = cratonvm_types::PointerMap::new();
        map.insert(from.as_ptr() as usize, to);
        map
    }

    fn orphans(ctx: &MockNativeContext) -> Vec<u32> {
        crate::io_orphan_fds()
            .lock()
            .get(&ctx.vm_identity())
            .cloned()
            .unwrap_or_default()
    }

    /// Where a real (hash-preserving) move of `obj` lands: the mock's identity
    /// hash is the low 32 bits of the address, so the move adds 4 GiB
    /// (gc-common w29-c; the rows are keyed by the owner's current address).
    fn moved_to(obj: ObjectRef) -> usize {
        obj.as_ptr() as usize + (1usize << 32)
    }

    /// The row key of `obj` after the sweep re-filed it at `to`.
    fn key_at(ctx: &MockNativeContext, obj: ObjectRef, to: usize) -> crate::IoRowKey {
        crate::io_row_key(crate::io_side_key(ctx, obj), to)
    }

    /// A reference at `addr`, for natives that only hash and compare their
    /// receiver (never read its fields: the mock heap does not know `addr`).
    fn at(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; callers pass 8-aligned addresses.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    // ------------------------------------------------------------ Scanner

    /// A `Scanner` stand-in over `text`: the fabricated model's five slots.
    fn scanner(ctx: &mut MockNativeContext, text: &str) -> ObjectRef {
        let this = ctx.alloc_object(5);
        crate::scanner_set_source(ctx, this, text);
        this
    }

    fn scanner_next(ctx: &mut MockNativeContext, this: ObjectRef) {
        let r = crate::native_scanner_next(ctx, &[Value::Object(Some(this))]);
        assert!(
            matches!(r, Ok(Some(Value::Object(Some(_))))),
            "next() gave {r:?}"
        );
    }

    /// `(input, owner)` of `key`'s `SCAN_SOURCES` row.
    fn source_row(key: crate::IoRowKey) -> Option<(String, usize)> {
        crate::scan_sources()
            .lock()
            .get(&key)
            .map(|row| (row.text.to_string(), row.owner))
    }

    /// The owner of `key`'s `SCAN_MATCHES` row.
    fn match_owner(key: crate::IoRowKey) -> Option<usize> {
        crate::scan_matches().lock().get(&key).map(|row| row.owner)
    }

    /// The retire criterion for rows 3 and 4: an unclosed scanner's input and
    /// last match used to stay resident for the life of the process. A moved
    /// scanner keeps both (re-addressed), a dead one loses both, and another
    /// VM's rows are never judged.
    #[test]
    fn scanner_sweep_follows_a_moved_scanner_and_drops_a_dead_ones_input_and_match() {
        let mut ctx = MockNativeContext::new();
        let mut other = MockNativeContext::new();
        let moved = scanner(&mut ctx, "a b");
        let dead = scanner(&mut ctx, "c d");
        let foreign = scanner(&mut other, "e f");
        scanner_next(&mut ctx, moved);
        scanner_next(&mut ctx, dead);
        scanner_next(&mut other, foreign);
        let moved_key = crate::io_side_row_key(&ctx, moved);
        let dead_key = crate::io_side_row_key(&ctx, dead);
        let foreign_key = crate::io_side_row_key(&other, foreign);
        assert_eq!(
            source_row(moved_key),
            Some(("a b".to_string(), moved.as_ptr() as usize))
        );
        assert_eq!(match_owner(dead_key), Some(dead.as_ptr() as usize));

        let to = moved_to(moved);
        let to_key = key_at(&ctx, moved, to);
        let dropped =
            crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map_one(moved, to), &|_| false);
        assert_eq!(dropped, 2, "the dead scanner's input and match");
        assert_eq!(source_row(to_key), Some(("a b".to_string(), to)));
        assert_eq!(match_owner(to_key), Some(to));
        assert_eq!(source_row(moved_key), None, "re-filed, not copied");
        assert_eq!(source_row(dead_key), None);
        assert_eq!(match_owner(dead_key), None);
        assert!(
            source_row(foreign_key).is_some(),
            "another VM's input is untouched"
        );
        assert!(
            match_owner(foreign_key).is_some(),
            "another VM's match is untouched"
        );
        forget(&[&ctx, &other]);
    }

    /// A live scanner the sweep kept still reads its input.
    #[test]
    fn scanner_kept_by_the_sweep_still_reads_its_input() {
        let mut ctx = MockNativeContext::new();
        let live = scanner(&mut ctx, "x y");
        let addr = live.as_ptr() as usize;
        let dropped = crate::gc_sweep_io_side_tables(
            ctx.vm_identity(),
            &cratonvm_types::PointerMap::new(),
            &|a| a == addr,
        );
        assert_eq!(dropped, 0);
        let src = crate::scanner_source(&mut ctx, live);
        assert_eq!(src.as_deref(), Some("x y"));
        forget(&[&ctx]);
    }

    // ------------------------------------------------------------ DatagramChannel

    fn open_udp(ctx: &MockNativeContext) -> Option<u32> {
        match ctx.fd_table().open_udp_dual_stack() {
            Ok(fd) => Some(fd),
            Err(e) => {
                eprintln!("SKIP: the host refused a UDP socket: {e}");
                None
            }
        }
    }

    /// Every DatagramChannel row a live, connected, non-blocking channel with
    /// one recorded option has.
    fn dc_fill(ctx: &MockNativeContext, ch: ObjectRef, fd: u32) {
        crate::set_dc_fd(ctx, ch, fd);
        crate::dc_mark_connected(ctx, ch);
        crate::dc_set_blocking(ctx, ch, false);
        crate::dc_option_set(ctx, ch, "SO_BROADCAST", 1);
    }

    /// The owners of `key`'s `(fd, connected, non-blocking, options)` rows.
    type DcOwners = (
        Option<(u32, usize)>,
        Option<usize>,
        Option<usize>,
        Option<usize>,
    );

    fn dc_owners(key: crate::IoRowKey) -> DcOwners {
        (
            crate::dc_fds().lock().get(&key).map(|r| (r.fd, r.owner)),
            crate::dc_connected_channels()
                .lock()
                .get(&key)
                .map(|r| r.owner),
            crate::dc_nonblocking_channels()
                .lock()
                .get(&key)
                .map(|r| r.owner),
            crate::dc_option_state().lock().get(&key).map(|r| r.owner),
        )
    }

    /// The retire criterion for row 5: a channel dropped without `close()`
    /// kept its rows AND its UDP socket for the life of the process. The sweep
    /// drops the dead channel's four rows and parks its socket; the next
    /// drain closes it. A moved channel keeps everything, re-addressed.
    #[test]
    fn dc_sweep_drops_a_dead_channel_closes_its_socket_and_follows_a_moved_one() {
        let mut ctx = MockNativeContext::new();
        let mut other = MockNativeContext::new();
        let fds: Vec<u32> = (0..3).filter_map(|_| open_udp(&ctx)).collect();
        if fds.len() < 3 {
            for fd in fds {
                let _ = ctx.fd_table().close(fd);
            }
            return;
        }
        let (fd_moved, fd_dead, fd_foreign) = (fds[0], fds[1], fds[2]);
        let moved = ctx.alloc_object(3);
        let dead = ctx.alloc_object(3);
        let foreign = other.alloc_object(3);
        dc_fill(&ctx, moved, fd_moved);
        dc_fill(&ctx, dead, fd_dead);
        dc_fill(&other, foreign, fd_foreign);
        let moved_key = crate::io_side_row_key(&ctx, moved);
        let dead_key = crate::io_side_row_key(&ctx, dead);
        let foreign_key = crate::io_side_row_key(&other, foreign);
        let was = moved.as_ptr() as usize;
        assert_eq!(
            dc_owners(moved_key),
            (Some((fd_moved, was)), Some(was), Some(was), Some(was))
        );

        let to = moved_to(moved);
        let dropped =
            crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map_one(moved, to), &|_| false);
        assert_eq!(
            dropped, 4,
            "the dead channel's fd, connected, non-blocking and option rows"
        );
        assert_eq!(
            dc_owners(key_at(&ctx, moved, to)),
            (Some((fd_moved, to)), Some(to), Some(to), Some(to))
        );
        assert_eq!(dc_owners(moved_key), (None, None, None, None), "re-filed, not copied");
        // The channel, seen where it now is (these helpers only hash and
        // compare it).
        let now = at(to);
        assert_eq!(crate::dc_fd(&ctx, now), Some(fd_moved));
        assert!(crate::dc_is_connected(&ctx, now));
        assert!(!crate::dc_is_blocking(&ctx, now));
        assert_eq!(crate::dc_option_get(&ctx, now, "SO_BROADCAST"), Some(1));
        assert_eq!(dc_owners(dead_key), (None, None, None, None));
        assert!(
            dc_owners(foreign_key).0.is_some(),
            "another VM's channel is untouched"
        );

        // Parked, not yet closed: the sweep has no context.
        assert_eq!(orphans(&ctx), vec![fd_dead]);
        assert!(ctx.fd_table().udp_local_addr(fd_dead).is_ok());
        crate::close_orphan_fds(&ctx);
        assert!(
            ctx.fd_table().udp_local_addr(fd_dead).is_err(),
            "the dead channel's socket is closed"
        );
        assert!(orphans(&ctx).is_empty());
        assert!(
            ctx.fd_table().udp_local_addr(fd_moved).is_ok(),
            "the live one is not"
        );
        assert!(ctx.fd_table().udp_local_addr(fd_foreign).is_ok());

        let _ = ctx.fd_table().close(fd_moved);
        let _ = ctx.fd_table().close(fd_foreign);
        forget(&[&ctx, &other]);
    }

    const ADAPTOR_CREATE: &str = "(Lsun/nio/ch/DatagramChannelImpl;)Ljava/net/DatagramSocket;";

    fn create_calls(ctx: &MockNativeContext) -> usize {
        // SAFETY: single-threaded test; no call is in flight and no other
        // borrow of the call log is live.
        unsafe { &*ctx.calls.get() }
            .iter()
            .filter(|c| c.method_name == "create" && c.descriptor == ADAPTOR_CREATE)
            .count()
    }

    /// The retire criterion for row 6: `socket()` kept the adaptor in a table
    /// holding TWO global roots (channel and adaptor), which made the channel
    /// immortal. On a real `DatagramChannelImpl` layout the adaptor now lives
    /// in the channel's own `socket` field, no root is taken, and the second
    /// `socket()` answers the same adaptor without creating another.
    #[test]
    fn dc_socket_keeps_its_adaptor_in_the_channels_socket_field_not_a_global_root() {
        let mut ctx = MockNativeContext::new();
        ctx.declare_field("sun/nio/ch/DatagramChannelImpl", "socket", 5);
        let adaptor = ctx.alloc_object(0);
        let ch = ctx.alloc_object_with_class(6, "sun/nio/ch/DatagramChannelImpl");
        ctx.script_invoke(
            "create",
            ADAPTOR_CREATE,
            Ok(Some(Value::Object(Some(adaptor)))),
        );

        let first = crate::native_dc_socket(&mut ctx, &[Value::Object(Some(ch))]);
        assert!(
            matches!(first, Ok(Some(Value::Object(Some(a)))) if a == adaptor),
            "first socket() gave {first:?}"
        );
        assert_eq!(ctx.get_field(ch, 5), Value::Object(Some(adaptor)));
        assert_eq!(ctx.global_root_count(), 0, "no global root");
        let key = crate::io_side_key(&ctx, ch);
        assert!(!crate::dc_socket_cache().lock().contains_key(&key));

        let second = crate::native_dc_socket(&mut ctx, &[Value::Object(Some(ch))]);
        assert!(matches!(second, Ok(Some(Value::Object(Some(a)))) if a == adaptor));
        assert_eq!(
            create_calls(&ctx),
            1,
            "the cached adaptor is answered, not re-created"
        );
        forget(&[&ctx]);
    }

    /// Without the field (a layout that is not the real `Impl`), the rooted
    /// cache is still the fallback, and teardown now drops its row.
    #[test]
    fn dc_socket_without_the_field_falls_back_to_the_cache_and_teardown_drops_it() {
        let mut ctx = MockNativeContext::new();
        let ch = ctx.alloc_object(3);
        let adaptor = ctx.alloc_object(0);
        ctx.script_invoke(
            "create",
            ADAPTOR_CREATE,
            Ok(Some(Value::Object(Some(adaptor)))),
        );
        let first = crate::native_dc_socket(&mut ctx, &[Value::Object(Some(ch))]);
        assert!(matches!(first, Ok(Some(Value::Object(Some(a)))) if a == adaptor));
        assert_eq!(ctx.global_root_count(), 2);
        let second = crate::native_dc_socket(&mut ctx, &[Value::Object(Some(ch))]);
        assert!(matches!(second, Ok(Some(Value::Object(Some(a)))) if a == adaptor));
        assert_eq!(create_calls(&ctx), 1);
        let key = crate::io_side_key(&ctx, ch);
        assert!(crate::dc_socket_cache().lock().contains_key(&key));
        assert_eq!(crate::forget_vm_io_side_tables(ctx.vm_identity()), 1);
        assert!(!crate::dc_socket_cache().lock().contains_key(&key));
    }

    // ------------------------------------------------------------ RandomAccessFile

    fn raf_file(ctx: &MockNativeContext, dir: &tempfile::TempDir, name: &str) -> u32 {
        let path = dir.path().join(name);
        ctx.fd_table()
            .open_read_write(path.to_str().expect("utf-8 path"), true)
            .expect("open")
    }

    /// The retire criterion for row 7: a synthetic `RandomAccessFile`
    /// dropped without `close()` kept its row and its fd. The sweep drops the
    /// row and parks the fd; the next drain closes it.
    #[test]
    fn raf_sweep_drops_a_dead_file_and_the_next_drain_closes_its_fd() {
        let mut ctx = MockNativeContext::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let fd_moved = raf_file(&ctx, &dir, "moved.bin");
        let fd_dead = raf_file(&ctx, &dir, "dead.bin");
        let moved = ctx.alloc_object(2);
        let dead = ctx.alloc_object(2);
        crate::raf_set_state(&ctx, moved, fd_moved, "moved.bin".to_string());
        crate::raf_set_state(&ctx, dead, fd_dead, "dead.bin".to_string());

        let to = moved_to(moved);
        let dropped =
            crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map_one(moved, to), &|_| false);
        assert_eq!(dropped, 1);
        assert_eq!(
            crate::raf_states()
                .lock()
                .get(&key_at(&ctx, moved, to))
                .map(|r| r.owner),
            Some(to)
        );
        assert_eq!(crate::raf_fd(&ctx, at(to)), Some(fd_moved));
        assert_eq!(crate::raf_fd(&ctx, moved), None, "nothing left at the old address");
        assert_eq!(crate::raf_fd(&ctx, dead), None);
        assert_eq!(orphans(&ctx), vec![fd_dead]);
        assert!(ctx.fd_table().rw_position(fd_dead).is_ok());
        crate::close_orphan_fds(&ctx);
        assert!(
            ctx.fd_table().rw_position(fd_dead).is_err(),
            "the dead file's fd is closed"
        );
        assert!(
            ctx.fd_table().rw_position(fd_moved).is_ok(),
            "the live one is not"
        );
        let _ = ctx.fd_table().close(fd_moved);
        forget(&[&ctx]);
    }

    /// Re-initialising a file's row replaces its fd; the replaced fd, which
    /// nothing will close any more, is parked like a dead file's.
    #[test]
    fn raf_reinit_parks_the_replaced_fd() {
        let mut ctx = MockNativeContext::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let fd_old = raf_file(&ctx, &dir, "old.bin");
        let fd_new = raf_file(&ctx, &dir, "new.bin");
        let f = ctx.alloc_object(2);
        crate::raf_set_state(&ctx, f, fd_old, "old.bin".to_string());
        crate::raf_set_state(&ctx, f, fd_new, "new.bin".to_string());
        assert_eq!(orphans(&ctx), vec![fd_old]);
        crate::close_orphan_fds(&ctx);
        assert!(ctx.fd_table().rw_position(fd_old).is_err());
        assert_eq!(crate::raf_fd(&ctx, f), Some(fd_new));
        let _ = ctx.fd_table().close(fd_new);
        forget(&[&ctx]);
    }

    // ------------------------------------------------------------ teardown

    /// `forget_vm_io_side_tables` covers every w16-c table and the VM's parked
    /// fds, only for its VM, and is idempotent. Fake fd ids: no row here is
    /// ever closed, and the shared counter never reaches them.
    #[test]
    fn forgetting_a_vm_drops_every_w16c_row_and_its_parked_fds() {
        let mut ctx = MockNativeContext::new();
        let mut other = MockNativeContext::new();
        let s = scanner(&mut ctx, "p q");
        scanner_next(&mut ctx, s);
        let ch = ctx.alloc_object(3);
        dc_fill(&ctx, ch, u32::MAX - 200);
        let f = ctx.alloc_object(2);
        crate::raf_set_state(&ctx, f, u32::MAX - 201, "f".to_string());
        crate::queue_orphan_fds(ctx.vm_identity(), vec![u32::MAX - 202]);
        let kept = scanner(&mut other, "kept");

        // 2 scanner + 4 channel + 1 file rows.
        assert_eq!(crate::forget_vm_io_side_tables(ctx.vm_identity()), 7);
        assert!(orphans(&ctx).is_empty(), "the parked fds go with the VM");
        assert_eq!(
            crate::forget_vm_io_side_tables(ctx.vm_identity()),
            0,
            "idempotent"
        );
        assert!(
            source_row(crate::io_side_row_key(&other, kept)).is_some(),
            "another VM keeps its row"
        );
        forget(&[&other]);
    }
}

/// gc-common w20-f: stale-handle triage of `native-io`. The mock's
/// [`MockNativeContext::w20f_relocate_on_invoke`] makes one Java call a moving
/// collection; a native that keeps using a Rust local across it instead of
/// re-reading it through a pin panics with "invalid ObjectRef".
///
/// The `#[cfg(test)]` is redundant (this whole file is `#[cfg(test)] mod
/// test_support` in `lib.rs`); it is what tells the stale-handle audit, which
/// cannot see a parent's `mod` attribute, that these are tests.
#[cfg(test)]
mod w20f_stale_handle_tests {
    use super::*;

    fn rooted(ctx: &MockNativeContext, root: usize) -> ObjectRef {
        ctx.resolve_global_root(root).expect("rooted")
    }

    /// `BufferedOutputStream.write(byte[], off, len)` with too little room
    /// flushes first -- `out.write(buf, 0, count)`, arbitrary Java -- and then
    /// copies the caller's array into the emptied buffer. The copy read the
    /// array at its address from before the flush.
    #[test]
    fn w20f_bos_bulk_write_copies_from_the_moved_source_after_its_flush() {
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(3);
        let out = ctx.alloc_object(0);
        let buf = ctx.new_array(ArrayElementType::Byte, 4);
        ctx.set_field(this, crate::BOS_FIELD_OUT, Value::Object(Some(out)));
        ctx.set_field(this, crate::BOS_FIELD_BUF, Value::Object(Some(buf)));
        ctx.set_field(this, crate::BOS_FIELD_COUNT, Value::Int(3));
        let src = ctx.new_array(ArrayElementType::Byte, 2);
        ctx.set_array_element(src, 0, Value::Int(7));
        ctx.set_array_element(src, 1, Value::Int(9));
        let this_root = ctx.add_global_root(this);
        ctx.w20f_relocate_on_invoke("write", "([BII)V");

        let r = crate::native_bos_write_bulk(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(src)),
                Value::Int(0),
                Value::Int(2),
            ],
        );

        assert!(matches!(r, Ok(None)), "got {r:?}");
        assert_eq!(
            ctx.relocation_count(),
            1,
            "the flush's write moved every object"
        );
        let moved = rooted(&ctx, this_root);
        assert!(matches!(
            ctx.get_field(moved, crate::BOS_FIELD_COUNT),
            Value::Int(2)
        ));
        let moved_buf = match ctx.get_field(moved, crate::BOS_FIELD_BUF) {
            Value::Object(Some(b)) => b,
            other => panic!("buf slot is {other:?}"),
        };
        assert_eq!(ctx.get_array_element(moved_buf, 0), Value::Int(7));
        assert_eq!(ctx.get_array_element(moved_buf, 1), Value::Int(9));
        assert_eq!(ctx.pinned_count(), 0, "no pin left behind");
    }
}

/// gc-common w29-c (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 8, route R2): two LIVE objects of ONE VM with EQUAL identity hashes
/// keep their own rows in every native-io weak side table. The tables were
/// keyed by `(vm_identity, identity hash)` alone, so such a pair shared one
/// row: a second `RandomAccessFile` or `DatagramChannel` REPLACED the first
/// one's descriptor (parking it for close), and the first then read, wrote
/// and sent through the second's.
///
/// The mock's identity hash is the address truncated to `i32`, so two
/// references 4 GiB apart share one; each test asserts that premise first.
/// The natives exercised here only hash and compare their receiver (they
/// never read its fields), so the references need not be mock-heap objects.
/// Every test runs on its own mock, i.e. its own VM identity, and forgets that
/// VM's rows at the end (in a Drop guard); fake fd ids are never closed.
mod w29c_same_hash_live_pair_tests {
    use super::*;

    const FOUR_GIB: usize = 1 << 32;

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; every address below is 8-aligned.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Two references of one VM with the same identity hash, the premise
    /// asserted.
    fn same_hash_pair(ctx: &MockNativeContext, base: usize) -> (ObjectRef, ObjectRef) {
        let (a, b) = (at(base), at(base + FOUR_GIB));
        assert_eq!(
            ctx.identity_hash_code(a),
            ctx.identity_hash_code(b),
            "the collision under test"
        );
        assert_ne!(a.as_ptr(), b.as_ptr());
        (a, b)
    }

    fn orphans(ctx: &MockNativeContext) -> Vec<u32> {
        crate::io_orphan_fds()
            .lock()
            .get(&ctx.vm_identity())
            .cloned()
            .unwrap_or_default()
    }

    /// Forgets the mock VM's rows (and parked fds) even when an assertion
    /// fails, so a failure cannot leak rows into the process-wide tables.
    struct Forget(usize);

    impl Drop for Forget {
        fn drop(&mut self) {
            crate::forget_vm_io_side_tables(self.0);
        }
    }

    /// A temp file holding `bytes`, opened read-write in the mock's fd table.
    fn rw_file(ctx: &MockNativeContext, dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> u32 {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write");
        ctx.fd_table()
            .open_read_write(path.to_str().expect("utf-8 path"), true)
            .expect("open")
    }

    fn raf_read(ctx: &mut MockNativeContext, this: ObjectRef) -> MethodCallResult {
        crate::native_raf_read(ctx, &[Value::Object(Some(this))])
    }

    /// The census's first test: two synthetic `RandomAccessFile`s with one
    /// identity hash each read and write THEIR OWN descriptor. The second
    /// constructor used to replace the first's row and park its fd.
    #[test]
    fn raf_same_hash_live_pair_keep_their_own_descriptors() {
        let mut ctx = MockNativeContext::new();
        let _forget = Forget(ctx.vm_identity());
        let (a, b) = same_hash_pair(&ctx, 0x2C29_1000);
        let dir = tempfile::tempdir().expect("tempdir");
        let fd_a = rw_file(&ctx, &dir, "a.bin", b"AAAA");
        let fd_b = rw_file(&ctx, &dir, "b.bin", b"BBBB");
        crate::raf_set_state(&ctx, a, fd_a, "a.bin".to_string());
        crate::raf_set_state(&ctx, b, fd_b, "b.bin".to_string());
        assert!(orphans(&ctx).is_empty(), "no live file's fd was replaced");
        assert_eq!(crate::raf_fd(&ctx, a), Some(fd_a));
        assert_eq!(crate::raf_fd(&ctx, b), Some(fd_b));

        assert!(matches!(raf_read(&mut ctx, a), Ok(Some(Value::Int(0x41)))));
        assert!(matches!(raf_read(&mut ctx, b), Ok(Some(Value::Int(0x42)))));
        let w = crate::native_raf_write(
            &mut ctx,
            &[Value::Object(Some(b)), Value::Int(b'x' as i32)],
        );
        assert!(matches!(w, Ok(None)), "write gave {w:?}");
        assert!(
            matches!(raf_read(&mut ctx, a), Ok(Some(Value::Int(0x41)))),
            "A's position is its own: B's write did not advance it"
        );

        // `close()` on B removes B's row only.
        let r = crate::native_raf_close(&mut ctx, &[Value::Object(Some(b))]);
        assert!(matches!(r, Ok(None)), "close gave {r:?}");
        assert_eq!(crate::raf_fd(&ctx, b), None);
        assert_eq!(crate::raf_fd(&ctx, a), Some(fd_a), "A stays open");
        assert!(matches!(raf_read(&mut ctx, a), Ok(Some(Value::Int(0x41)))));
        let _ = ctx.fd_table().close(fd_a);
    }

    /// Killing one of the pair drops only its row (and parks only its fd); a
    /// hash-preserving move of the survivor is followed, and the survivor is
    /// found where it now is.
    #[test]
    fn raf_same_hash_pair_sweep_drops_the_dead_one_and_follows_the_live_one() {
        let ctx = MockNativeContext::new();
        let _forget = Forget(ctx.vm_identity());
        let (a, b) = same_hash_pair(&ctx, 0x2C29_2000);
        let (fd_a, fd_b) = (u32::MAX - 290, u32::MAX - 291);
        crate::raf_set_state(&ctx, a, fd_a, "a".to_string());
        crate::raf_set_state(&ctx, b, fd_b, "b".to_string());

        let b_addr = b.as_ptr() as usize;
        let empty = cratonvm_types::PointerMap::new();
        let dropped = crate::gc_sweep_io_side_tables(ctx.vm_identity(), &empty, &|x| x != b_addr);
        assert_eq!(dropped, 1, "only B died");
        assert_eq!(crate::raf_fd(&ctx, b), None);
        assert_eq!(crate::raf_fd(&ctx, a), Some(fd_a), "A's row survives B's death");
        assert_eq!(orphans(&ctx), vec![fd_b], "only B's fd is parked");

        // A moves (keeping its hash) onto a band B never used.
        let a_new = a.as_ptr() as usize + 2 * FOUR_GIB;
        let mut map = cratonvm_types::PointerMap::new();
        map.insert(a.as_ptr() as usize, a_new);
        assert_eq!(crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map, &|_| false), 0);
        assert_eq!(crate::raf_fd(&ctx, at(a_new)), Some(fd_a));
        assert_eq!(crate::raf_fd(&ctx, a), None, "nothing left at the old address");
    }

    /// The re-filing sweep's two hard cases, in one bucket: a chain of moves
    /// (x to where y was, y onward) re-files each row once, and a row found at
    /// a moved row's destination (an object that died there) is displaced --
    /// dropped and its fd parked -- never judged or kept.
    #[test]
    fn sweep_refiles_a_chain_of_moves_and_displaces_a_dead_row_at_a_destination() {
        let ctx = MockNativeContext::new();
        let _forget = Forget(ctx.vm_identity());
        let base = 0x2C29_3000usize;
        let (x, y, z) = (base, base + FOUR_GIB, base + 2 * FOUR_GIB);
        let dead = base + 3 * FOUR_GIB;
        let w = base + 4 * FOUR_GIB;
        let (fd_x, fd_y, fd_dead, fd_w) = (
            u32::MAX - 300,
            u32::MAX - 301,
            u32::MAX - 302,
            u32::MAX - 303,
        );
        crate::raf_set_state(&ctx, at(x), fd_x, "x".to_string());
        crate::raf_set_state(&ctx, at(y), fd_y, "y".to_string());
        crate::raf_set_state(&ctx, at(dead), fd_dead, "dead".to_string());
        crate::raf_set_state(&ctx, at(w), fd_w, "w".to_string());

        // x -> y (y's row moves on to z); w -> dead (dead's object is gone).
        let mut map = cratonvm_types::PointerMap::new();
        map.insert(x, y);
        map.insert(y, z);
        map.insert(w, dead);
        let dropped = crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map, &|_| true);
        assert_eq!(dropped, 1, "the displaced row, even with a live verdict for everything");
        assert_eq!(crate::raf_fd(&ctx, at(y)), Some(fd_x), "x's row followed x");
        assert_eq!(crate::raf_fd(&ctx, at(z)), Some(fd_y), "y's row followed y");
        assert_eq!(crate::raf_fd(&ctx, at(dead)), Some(fd_w), "w's row took the dead one's place");
        assert_eq!(crate::raf_fd(&ctx, at(x)), None);
        assert_eq!(crate::raf_fd(&ctx, at(w)), None);
        assert_eq!(orphans(&ctx), vec![fd_dead], "the displaced row's fd is parked");
    }

    fn open_udp(ctx: &MockNativeContext) -> Option<u32> {
        match ctx.fd_table().open_udp_dual_stack() {
            Ok(fd) => Some(fd),
            Err(e) => {
                eprintln!("SKIP: the host refused a UDP socket: {e}");
                None
            }
        }
    }

    /// The census's second test: two `DatagramChannel`s with one identity
    /// hash keep their own socket, connected / blocking state and options.
    /// The second `open()` used to replace the first's socket (parking it),
    /// after which the first sent and received on the second's.
    #[test]
    fn dc_same_hash_live_pair_keep_their_own_sockets_and_state() {
        let ctx = MockNativeContext::new();
        let _forget = Forget(ctx.vm_identity());
        let (a, b) = same_hash_pair(&ctx, 0x2C29_4000);
        let Some(fd_a) = open_udp(&ctx) else {
            return;
        };
        let Some(fd_b) = open_udp(&ctx) else {
            let _ = ctx.fd_table().close(fd_a);
            return;
        };
        crate::set_dc_fd(&ctx, a, fd_a);
        crate::set_dc_fd(&ctx, b, fd_b);
        assert!(orphans(&ctx).is_empty(), "no live channel's socket was replaced");
        assert_eq!(crate::dc_fd(&ctx, a), Some(fd_a));
        assert_eq!(crate::dc_fd(&ctx, b), Some(fd_b));

        crate::dc_mark_connected(&ctx, a);
        assert!(crate::dc_is_connected(&ctx, a));
        assert!(!crate::dc_is_connected(&ctx, b), "A's connect is not B's");
        crate::dc_set_blocking(&ctx, b, false);
        assert!(!crate::dc_is_blocking(&ctx, b));
        assert!(crate::dc_is_blocking(&ctx, a), "B's mode is not A's");
        crate::dc_option_set(&ctx, a, "SO_BROADCAST", 1);
        assert_eq!(crate::dc_option_get(&ctx, a, "SO_BROADCAST"), Some(1));
        assert_eq!(crate::dc_option_get(&ctx, b, "SO_BROADCAST"), None);

        // Closing B's socket leaves A's.
        assert_eq!(crate::remove_dc_fd(&ctx, b), Some(fd_b));
        assert_eq!(crate::dc_fd(&ctx, a), Some(fd_a), "A keeps its socket");
        assert!(ctx.fd_table().udp_local_addr(fd_a).is_ok());
        let _ = ctx.fd_table().close(fd_a);
        let _ = ctx.fd_table().close(fd_b);
    }

    /// A reader table: two `StringReader`s with one identity hash read their
    /// own content, and one's `close()` does not close the other. `<init>` of
    /// the second used to REPLACE the first's content.
    #[test]
    fn sr_same_hash_live_pair_read_their_own_content() {
        fn read(ctx: &mut MockNativeContext, this: ObjectRef) -> MethodCallResult {
            crate::native_sr_read(ctx, &[Value::Object(Some(this))])
        }
        let mut ctx = MockNativeContext::new();
        let _forget = Forget(ctx.vm_identity());
        let (a, b) = same_hash_pair(&ctx, 0x2C29_5000);
        for (this, text) in [(a, "alpha"), (b, "beta")] {
            let s = ctx.create_string(text);
            let r = crate::native_sr_init(
                &mut ctx,
                &[Value::Object(Some(this)), Value::Object(Some(s))],
            );
            assert!(matches!(r, Ok(None)), "init gave {r:?}");
        }
        assert!(matches!(read(&mut ctx, a), Ok(Some(Value::Int(0x61)))));
        assert!(matches!(read(&mut ctx, b), Ok(Some(Value::Int(0x62)))));
        let r = crate::native_sr_close(&mut ctx, &[Value::Object(Some(b))]);
        assert!(matches!(r, Ok(None)));
        assert!(read(&mut ctx, b).is_err(), "B is closed");
        assert!(
            matches!(read(&mut ctx, a), Ok(Some(Value::Int(0x6C)))),
            "A is open and reads its own 'l'"
        );
    }

    /// The side buffers: two streams with one identity hash buffer into their
    /// own rows (the old key made them buffer into, and flush, one row).
    #[test]
    fn bos_same_hash_live_pair_buffer_their_own_bytes() {
        let mut ctx = MockNativeContext::new();
        let _forget = Forget(ctx.vm_identity());
        let (a, b) = same_hash_pair(&ctx, 0x2C29_6000);
        crate::bos_side_init(&ctx, a, 8);
        crate::bos_side_init(&ctx, b, 8);
        // Below capacity, so no flush (which would read the streams' fields).
        for (this, byte) in [(a, b'a'), (b, b'b'), (a, b'c')] {
            let r = crate::bos_side_write_byte(&mut ctx, this, 0, byte as i32);
            assert!(matches!(r, Ok(None)), "write gave {r:?}");
        }
        let bytes = |ctx: &MockNativeContext, this: ObjectRef| {
            crate::bos_side_buffers()
                .lock()
                .get(&crate::io_side_row_key(ctx, this))
                .map(|row| row.bytes.clone())
        };
        assert_eq!(bytes(&ctx, a), Some(b"ac".to_vec()));
        assert_eq!(bytes(&ctx, b), Some(b"b".to_vec()));
    }
}
