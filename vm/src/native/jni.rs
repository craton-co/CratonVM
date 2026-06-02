// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JNI (Java Native Interface) implementation.
//!
//! Provides the standard C API for native code to interact with the JVM.
//! The function table is represented as a flat `[usize; 229]` array matching
//! the JNI spec layout. Native code accesses it via `(*env)[index](env, ...)`.
//!
//! JNI functions access the VM via thread-local storage. Before calling into
//! native code, the interpreter sets the TLS context with `set_jni_context()`,
//! and clears it on return with `clear_jni_context()`.

#![allow(dead_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::Arc;

use crate::classloading::{find_field_recursive, find_method_recursive, ClassId};
use crate::memory::heap::ArrayElementType;
use crate::threading::{JvmThread, ThreadId};
use crate::types::{ObjectRef, Value};
use crate::vm::{create_java_string, invoke_on_class_shared, read_java_string, SharedVm};

// ---------------------------------------------------------------------------
// JNI type aliases
//
// These are bare `u64` aliases rather than newtypes for C ABI compatibility
// across hundreds of `extern "C"` call sites.  They represent distinct JNI
// handle types at the conceptual level:
//
//   JObject     – opaque handle to a Java object (0 ≡ null)
//   JClass      – handle to a `java.lang.Class` instance
//   JString     – handle to a `java.lang.String` instance
//   JArray      – handle to a Java array
//   JThrowable  – handle to a `java.lang.Throwable` instance
//   JMethodID   – opaque identifier for a resolved method
//   JFieldID    – opaque identifier for a resolved field
// ---------------------------------------------------------------------------

pub type JBoolean = u8;
pub type JByte = i8;
pub type JChar = u16;
pub type JShort = i16;
pub type JInt = i32;
pub type JLong = i64;
pub type JFloat = f32;
pub type JDouble = f64;
pub type JSize = i32;

/// Opaque handle to a Java object. A value of `0` represents JNI `NULL`.
pub type JObject = u64;
/// Alias for [`JObject`] — handle to a `java.lang.Class`.
pub type JClass = JObject;
/// Alias for [`JObject`] — handle to a `java.lang.String`.
pub type JString = JObject;
/// Alias for [`JObject`] — handle to a Java array.
pub type JArray = JObject;
/// Alias for [`JObject`] — handle to a `java.lang.Throwable`.
pub type JThrowable = JObject;
/// Opaque method identifier obtained from `GetMethodID`/`GetStaticMethodID`.
pub type JMethodID = u64;
/// Opaque field identifier obtained from `GetFieldID`/`GetStaticFieldID`.
pub type JFieldID = u64;

/// JavaVM is a pointer to a pointer to the invocation function table.
pub type JavaVM = *const *const usize;

/// JNIEnv is a pointer to a pointer to the function table (per JNI spec).
pub type JNIEnv = *const *const usize;

pub const JNI_VERSION_1_8: JInt = 0x00010008;
pub const JNI_OK: JInt = 0;
pub const JNI_ERR: JInt = -1;
pub const JNI_FALSE: JBoolean = 0;
pub const JNI_TRUE: JBoolean = 1;
pub const JNI_FUNCTION_COUNT: usize = 234;
pub const JNI_INVOKE_FUNCTION_COUNT: usize = 8;

// ---------------------------------------------------------------------------
// Thread-Local VM Context
// ---------------------------------------------------------------------------

thread_local! {
    /// Holds an `Arc<SharedVm>` while inside a JNI native call.
    ///
    /// Using `Arc` instead of a raw pointer prevents use-after-free: the
    /// reference count keeps the `SharedVm` alive for the entire duration of the
    /// native call, even if other `Arc` holders drop their references.
    static JNI_SHARED_VM: std::cell::RefCell<Option<Arc<SharedVm>>> =
        std::cell::RefCell::new(None);
    static JNI_THREAD: Cell<*mut ()> = const { Cell::new(std::ptr::null_mut()) };
    static JNI_PENDING_EXCEPTION: Cell<u64> = const { Cell::new(0) };
    /// Generation counter incremented on every set/clear cycle.
    /// Allows detecting stale context in nested native calls.
    static JNI_CONTEXT_GENERATION: Cell<u64> = const { Cell::new(0) };
    /// Tracks (pointer -> element count) for UTF-16 buffers handed out by
    /// `GetStringChars` / `GetStringCritical` so that `ReleaseStringChars`
    /// can reconstruct the correct `Vec` layout and deallocate safely.
    static JNI_STRING_BUFFERS: std::cell::RefCell<HashMap<usize, usize>> =
        std::cell::RefCell::new(HashMap::new());
    /// Tracks temporary contiguous buffers handed out by
    /// `GetPrimitiveArrayCritical` when the underlying array is a G1
    /// **humongous** array (whose payload is split across non-contiguous
    /// regions and therefore has no flat data pointer). The buffer is a
    /// boxed `Vec<u8>` we materialised by copying every element in; the map
    /// records the metadata `ReleasePrimitiveArrayCritical` needs to copy
    /// the (possibly mutated) bytes back into the array and free the buffer.
    /// Entries are keyed by the returned pointer (`buf.as_mut_ptr() as usize`).
    static JNI_CRITICAL_COPIES: std::cell::RefCell<HashMap<usize, CriticalCopy>> =
        std::cell::RefCell::new(HashMap::new());
    /// Thread-local cache for parsed method descriptors.
    /// Maps descriptor string → parsed parameter type tags, avoiding
    /// repeated parsing of the same descriptor in hot JNI call paths.
    static JNI_DESCRIPTOR_CACHE: std::cell::RefCell<HashMap<String, Vec<u8>>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Set the JNI thread-local context before entering native code.
///
/// Stores an `Arc<SharedVm>` in thread-local storage, keeping the VM alive
/// via reference counting for the entire duration of the native call.
/// This eliminates the use-after-free risk of the previous raw-pointer design.
///
/// `clear_jni_context()` MUST be called after every native call returns
/// (including on panic/unwind paths) to release the `Arc` and allow the VM
/// to be dropped when no longer needed.
pub fn set_jni_context(shared: &SharedVm) {
    // Obtain an Arc from the SharedVm's weak self-reference.
    // This keeps the VM alive via ref-counting for the native call duration.
    let arc = shared.get_arc();
    JNI_SHARED_VM.with(|c| {
        *c.borrow_mut() = Some(arc);
    });
    JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}

/// Set JNI context directly from an existing `Arc<SharedVm>`.
/// Preferred when the caller already holds an Arc (avoids weak-reference upgrade).
pub fn set_jni_context_arc(shared: Arc<SharedVm>) {
    JNI_SHARED_VM.with(|c| {
        *c.borrow_mut() = Some(shared);
    });
    JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}

/// Clear the JNI thread-local context after returning from native code.
/// Drops the `Arc<SharedVm>`, decrementing the reference count.
pub fn clear_jni_context() {
    JNI_SHARED_VM.with(|c| {
        *c.borrow_mut() = None;
    });
    JNI_CONTEXT_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}

/// Set the JNI thread-local `JvmThread` pointer before entering native code.
///
/// # Safety
///
/// `thread` must point to a valid, exclusively-accessible `JvmThread` for
/// the entire duration of the native call.  The pointer is erased to
/// `*mut ()` in TLS and cast back in `with_jni_thread`, so the caller must
/// guarantee the pointee is not moved, dropped, or concurrently mutated
/// until `clear_jni_thread()` is called.
pub fn set_jni_thread(thread: *mut JvmThread) {
    JNI_THREAD.with(|c| c.set(thread as *mut ()));
}

/// Clear the JNI thread pointer on native code return.
pub fn clear_jni_thread() {
    JNI_THREAD.with(|c| c.set(std::ptr::null_mut()));
}

/// Take (read + clear) the pending JNI exception, if any.
///
/// Returns `Some(raw_value)` if an exception was set via `Throw` or `ThrowNew`,
/// and clears it so subsequent calls return `None`. The raw value is the
/// JNI-level handle (u64) — `u64::MAX` is the sentinel for `ThrowNew`.
pub fn take_jni_pending_exception() -> Option<u64> {
    JNI_PENDING_EXCEPTION.with(|cell| {
        let val = cell.get();
        if val != 0 {
            cell.set(0);
            Some(val)
        } else {
            None
        }
    })
}

/// Access the VM context from within a JNI function. Returns None if not set.
///
/// The `Arc<SharedVm>` in TLS guarantees the VM is alive for the entire
/// duration of the closure — no raw-pointer cast, no use-after-free risk.
fn with_shared_vm<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&SharedVm) -> R,
{
    JNI_SHARED_VM.with(|c| {
        let borrow = c.borrow();
        match borrow.as_ref() {
            None => None,
            Some(arc) => {
                let gen_before = JNI_CONTEXT_GENERATION.with(|g| g.get());
                let result = f(arc);
                let gen_after = JNI_CONTEXT_GENERATION.with(|g| g.get());
                if gen_before != gen_after {
                    tracing::warn!("JNI context generation changed during callback ({} -> {}) — possible nested native call", gen_before, gen_after);
                }
                Some(result)
            }
        }
    })
}

/// Access both SharedVm and JvmThread from within a JNI function.
/// The JvmThread borrow is exclusive for the duration of the closure.
/// Returns None if either context is not set.
fn with_jni_context<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&SharedVm, &mut JvmThread) -> R,
{
    let shared_arc: Option<Arc<SharedVm>> =
        JNI_SHARED_VM.with(|c| c.borrow().clone());
    let thread_ptr = JNI_THREAD.with(|c| c.get());
    match (shared_arc, thread_ptr.is_null()) {
        (Some(ref arc), false) => {
            // Safety: JNI_THREAD is set to a valid JvmThread pointer by set_jni_thread
            // and cleared by clear_jni_thread. The pointer is only used from the owning
            // thread (thread-local), and we hold exclusive access for the closure duration.
            Some(f(arc, unsafe { &mut *(thread_ptr as *mut JvmThread) }))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// JValue union (mirrors C `jvalue` union from jni.h)
// ---------------------------------------------------------------------------

/// C-compatible union of all JNI primitive and reference types.
/// Used by the CallXxxMethodA / CallStaticXxxMethodA families.
#[repr(C)]
pub union JValue {
    pub z: JBoolean, // boolean
    pub b: JByte,    // byte
    pub c: JChar,    // char
    pub s: JShort,   // short
    pub i: JInt,     // int
    pub j: JLong,    // long
    pub f: JFloat,   // float
    pub d: JDouble,  // double
    pub l: JObject,  // object / array reference
}

// ---------------------------------------------------------------------------
// Descriptor parsing helpers
// ---------------------------------------------------------------------------

/// Look up parsed parameter types from the thread-local cache, or parse and cache.
fn parse_param_types_cached(descriptor: &str) -> Vec<u8> {
    JNI_DESCRIPTOR_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(cached) = cache.get(descriptor) {
            return cached.clone();
        }
        let types = parse_param_types_inner(descriptor);
        // Cap cache size to avoid unbounded growth in long-running JVMs
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(descriptor.to_owned(), types.clone());
        types
    })
}

/// Parse the parameter type tags from a JNI method descriptor, e.g.
/// `(ILjava/lang/String;[BZ)V` → `[b'I', b'L', b'[', b'Z']`.
/// Object types (`L...;`) and array types (`[...`) are both returned as a
/// single token (`b'L'` and `b'['` respectively).
fn parse_param_types_inner(descriptor: &str) -> Vec<u8> {
    let mut types = Vec::new();
    let bytes = descriptor.as_bytes();
    let mut i = 1; // skip leading '('
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => {
                types.push(bytes[i]);
                i += 1;
            }
            b'L' => {
                types.push(b'L');
                i += 1;
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1; // skip ';'
            }
            b'[' => {
                types.push(b'[');
                i += 1;
                // Skip the element type (may itself be an object or array)
                if i < bytes.len() && bytes[i] == b'L' {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1; // skip ';'
                } else if i < bytes.len() && bytes[i] == b'[' {
                    // multi-dimensional: skip extra '[' characters
                    while i < bytes.len() && bytes[i] == b'[' {
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == b'L' {
                        i += 1;
                        while i < bytes.len() && bytes[i] != b';' {
                            i += 1;
                        }
                        i += 1;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1; // primitive element type
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    types
}

/// Convert a slice of `JValue`s to `Value`s using the type tags from
/// `parse_param_types`.  Returns `None` if `args` is null and `types` is
/// non-empty.
unsafe fn jvalues_to_values(
    args: *const JValue,
    types: &[u8],
) -> Vec<Value> {
    if args.is_null() {
        return Vec::new();
    }
    types
        .iter()
        .enumerate()
        .map(|(idx, &tag)| {
            let jv = &*args.add(idx);
            match tag {
                b'Z' => Value::Int(jv.z as i32),
                b'B' => Value::Int(jv.b as i32),
                b'C' => Value::Int(jv.c as i32),
                b'S' => Value::Int(jv.s as i32),
                b'I' => Value::Int(jv.i),
                b'J' => Value::Long(jv.j),
                b'F' => Value::Float(jv.f),
                b'D' => Value::Double(jv.d),
                _ /* b'L' | b'[' */ => match jobject_to_obj(jv.l) {
                    Some(r) => Value::Object(Some(r)),
                    None => Value::Object(None),
                },
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Core call helpers used by all Call*MethodA variants
// ---------------------------------------------------------------------------

/// Perform a virtual instance method call from JNI.
/// `obj` is the receiver; `mid` encodes (declaring_class_id, method_index);
/// `args` is the JValue array (may be null for zero-arg methods).
fn jni_call_instance(
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> Option<Value> {
    if obj == 0 || mid == 0 {
        return None;
    }
    with_jni_context(|shared, thread| {
        let oref = jobject_to_obj(obj)?;
        let obj_class_id = shared.heap.class_id_of(oref);
        let (decl_class_id, method_index) = decode_method_id(mid);
        let (method_name, descriptor) = {
            let cm = shared.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let mut jvm_args = Vec::with_capacity(1 + param_types.len());
        jvm_args.push(Value::Object(Some(oref)));
        jvm_args.extend(unsafe { jvalues_to_values(args, &param_types) });
        invoke_on_class_shared(shared, thread, obj_class_id, &method_name, &descriptor, &jvm_args)
            .ok()
            .flatten()
    })
    .flatten()
}

/// Perform a nonvirtual instance method call from JNI (dispatch on `clazz`,
/// not on the runtime type of `obj`).
fn jni_call_nonvirtual(
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> Option<Value> {
    if obj == 0 || mid == 0 {
        return None;
    }
    with_jni_context(|shared, thread| {
        let oref = jobject_to_obj(obj)?;
        let dispatch_class_id = if clazz != 0 {
            ClassId::new(clazz as u32)
        } else {
            let (dcid, _) = decode_method_id(mid);
            dcid
        };
        let (_, method_index) = decode_method_id(mid);
        let (method_name, descriptor) = {
            let cm = shared.class_manager.read();
            let class = cm.class_store.get(dispatch_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let mut jvm_args = Vec::with_capacity(1 + param_types.len());
        jvm_args.push(Value::Object(Some(oref)));
        jvm_args.extend(unsafe { jvalues_to_values(args, &param_types) });
        invoke_on_class_shared(
            shared,
            thread,
            dispatch_class_id,
            &method_name,
            &descriptor,
            &jvm_args,
        )
        .ok()
        .flatten()
    })
    .flatten()
}

/// Perform a static method call from JNI.
fn jni_call_static(
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> Option<Value> {
    if mid == 0 {
        return None;
    }
    with_jni_context(|shared, thread| {
        let (decl_class_id, method_index) = decode_method_id(mid);
        let class_id = if clazz != 0 {
            ClassId::new(clazz as u32)
        } else {
            decl_class_id
        };
        let (method_name, descriptor) = {
            let cm = shared.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let jvm_args = unsafe { jvalues_to_values(args, &param_types) };
        invoke_on_class_shared(shared, thread, class_id, &method_name, &descriptor, &jvm_args)
            .ok()
            .flatten()
    })
    .flatten()
}

// ---------------------------------------------------------------------------
// Global Reference Table
// ---------------------------------------------------------------------------
//
// Design: each global ref is stored in a heap-allocated `Box<ObjectRef>`.
// The `JObject` handle returned to native code has bit 0 set (tag bit) to
// distinguish global refs from ordinary local refs (raw heap pointers, which
// are at least 8-byte aligned so their bit 0 is always 0).
//
// Global ref handle encoding:
//   `handle = (Box::into_raw(box_ptr) as usize) | 1`
//
// Decoding (in `jobject_to_obj`):
//   if `handle & 1 == 1` → `*(handle & !1) as *const ObjectRef`
//   else                 → `handle` is a raw heap pointer

pub struct JniGlobalRefs {
    /// Raw `Box<ObjectRef>` pointers (stored as usize for Send/Sync).
    /// Each entry owns its allocation until `remove` is called.
    entries: Vec<usize>,
}

// Safety: `JniGlobalRefs` is stored behind `parking_lot::Mutex<>` in `SharedVm`.
// All mutations (`add`, `remove`, `update_after_gc`) require `&mut self`, enforced by
// the Mutex.  All reads (`resolve`, `collect_roots`, `count`) also go through the
// Mutex lock.  Critically, `jobject_to_obj()` acquires the same Mutex when resolving
// global ref handles, preventing a use-after-free race between concurrent `resolve()`
// and `remove()` calls.
unsafe impl Send for JniGlobalRefs {}
unsafe impl Sync for JniGlobalRefs {}

impl JniGlobalRefs {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Create a global ref for `obj`. Returns a tagged `JObject` handle.
    pub fn add(&mut self, obj: ObjectRef) -> JObject {
        let boxed: Box<ObjectRef> = Box::new(obj);
        let raw = Box::into_raw(boxed) as usize; // OWNERSHIP: transferred to self.entries, freed by JniGlobalRefs::remove() or Drop impl
        self.entries.push(raw);
        (raw | 1) as JObject
    }

    /// Release a global ref created by `add`. Returns `true` if found and removed.
    pub fn remove(&mut self, handle: JObject) -> bool {
        if handle & 1 == 0 {
            return false; // not a global ref handle
        }
        let raw = (handle & !1) as usize;
        if let Some(pos) = self.entries.iter().position(|&e| e == raw) {
            self.entries.swap_remove(pos);
            // Safety: raw was created by Box::into_raw and we own it.
            unsafe { drop(Box::from_raw(raw as *mut ObjectRef)) };
            true
        } else {
            tracing::debug!("JNI remove: global ref handle {handle:#x} not found");
            false
        }
    }

    /// Resolve a global ref handle to the referenced ObjectRef.
    /// Returns `None` if `handle` is 0 or is not a valid global ref.
    pub fn resolve(&self, handle: JObject) -> Option<ObjectRef> {
        if handle == 0 || handle & 1 == 0 {
            return None;
        }
        let raw = (handle & !1) as usize;
        // Validate the pointer is still tracked to prevent use-after-free.
        if !self.entries.contains(&raw) {
            tracing::warn!("JNI resolve: handle {handle:#x} not found in global ref table");
            return None;
        }
        // Safety: raw was created by Box::into_raw and is still in self.entries.
        Some(unsafe { *(raw as *const ObjectRef) })
    }

    /// How many active global refs exist.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Collect all referenced ObjectRefs into `out` (for GC root scanning).
    pub fn collect_roots(&self, out: &mut Vec<ObjectRef>) {
        for &raw in &self.entries {
            // Safety: raw is a valid Box<ObjectRef> pointer that we own.
            out.push(unsafe { *(raw as *const ObjectRef) });
        }
    }

    /// Apply a GC pointer map: update all stored ObjectRefs to their new addresses.
    pub fn update_after_gc(&mut self, pointer_map: &std::collections::HashMap<usize, usize>) {
        for &raw in &self.entries {
            // Safety: raw is a valid Box<ObjectRef> pointer that we own.
            let slot = raw as *mut ObjectRef;
            let old_obj = unsafe { *slot };
            let old_addr = old_obj.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                unsafe { *slot = ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
}

impl Default for JniGlobalRefs {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for JniGlobalRefs {
    fn drop(&mut self) {
        for raw in self.entries.drain(..) {
            // Safety: raw was created by Box::into_raw and we own it.
            unsafe { drop(Box::from_raw(raw as *mut ObjectRef)) };
        }
    }
}

// ---------------------------------------------------------------------------
// Local Reference Frame Stack
// ---------------------------------------------------------------------------
//
// JNI local refs are raw heap pointers (bit 0 = 0). Native code can call
// PushLocalFrame/PopLocalFrame to scope their lifetime. We maintain a per-
// thread stack of frames in JNI TLS; each frame is a Vec<JObject>.
//
// Thread-local local frame stack — used only while JNI is active.

thread_local! {
    static JNI_LOCAL_FRAMES: std::cell::RefCell<Vec<Vec<JObject>>> =
        std::cell::RefCell::new(Vec::new());
}

/// Push a new local frame onto this thread's local frame stack.
pub fn push_local_frame(capacity: usize) {
    JNI_LOCAL_FRAMES.with(|f| {
        f.borrow_mut().push(Vec::with_capacity(capacity.max(16)));
    });
}

/// Pop the topmost local frame, returning `result` (a JObject to promote to
/// the parent frame). Returns 0 if there is no active frame.
pub fn pop_local_frame(result: JObject) -> JObject {
    JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        let _ = stack.pop(); // drop all refs in the top frame
        result // caller-provided result is promoted to the parent frame (or kept as-is)
    })
}

/// Record a local ref in the current top frame.
/// If there is no active frame, the ref is untracked (still valid; auto-freed on JNI return).
pub fn track_local_ref(jobj: JObject) {
    if jobj == 0 {
        return;
    }
    JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        if let Some(top) = stack.last_mut() {
            top.push(jobj);
        }
    });
}

/// Remove a local ref from the current top frame (for DeleteLocalRef).
pub fn delete_local_ref(jobj: JObject) {
    if jobj == 0 {
        return;
    }
    JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        if let Some(top) = stack.last_mut() {
            top.retain(|&r| r != jobj);
        }
    });
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

pub fn obj_to_jobject(obj: ObjectRef) -> JObject {
    obj.as_ptr() as u64
}

/// Resolve a `JObject` to the underlying `ObjectRef`.
///
/// Two kinds of `JObject`:
///  - **Local ref** (bit 0 = 0): direct raw pointer to a heap object.
///  - **Global ref** (bit 0 = 1): tagged pointer to a `Box<ObjectRef>` in
///    `JniGlobalRefs`; dereference to get the actual ObjectRef.
///
/// For global refs, this function acquires the `jni_global_refs` Mutex via the
/// thread-local `JNI_SHARED_VM` to validate the handle is still alive before
/// dereferencing. This prevents a use-after-free race where a concurrent
/// `remove()` could deallocate the `Box` while we read it.
pub fn jobject_to_obj(jobj: JObject) -> Option<ObjectRef> {
    if jobj == 0 {
        None
    } else if jobj & 1 == 1 {
        // Global ref: resolve through the locked table to prevent
        // use-after-free if another thread concurrently calls remove().
        JNI_SHARED_VM.with(|c| {
            let borrow = c.borrow();
            match borrow.as_ref() {
                Some(shared) => shared.jni_global_refs.lock().resolve(jobj),
                None => {
                    tracing::warn!(
                        "jobject_to_obj: global ref {jobj:#x} resolved outside JNI context"
                    );
                    None
                }
            }
        })
    } else {
        // Local ref: raw heap pointer.
        //
        // SECURITY FIX (V3): Previously this branch blindly reconstructed an
        // ObjectRef from the caller-supplied pointer with no validation, while
        // the global-ref branch above is protected by a locked table lookup. A
        // forged native pointer would become a wild read/write, and a local ref
        // held across a GC safepoint could be a stale from-space pointer under
        // the moving/generational collector. Mirror the global-ref path's
        // defensive posture: validate the address against the live heap (the
        // same `heap.is_heap_addr` check the GC root scanners use) and return
        // `None` on failure. `is_heap_addr` returns a freshly reconstructed
        // ObjectRef for a confirmed-live, aligned heap address, so we use its
        // result directly instead of an unchecked `from_raw`.
        //
        // The heap is reached through the same thread-local `JNI_SHARED_VM`
        // context the global-ref path uses (`SharedVm::heap`).
        //
        // FOLLOW-UP: the long-term fix is a full per-thread JNI local-handle
        // table (indirection handles validated on every access, like HotSpot's
        // JNIHandleBlock) so a local jobject can never be a raw heap pointer at
        // all. This address-validity gate is the minimum viable mitigation; it
        // does not catch a forged pointer that happens to land on a live
        // object, which only the handle table would fully prevent.
        JNI_SHARED_VM.with(|c| {
            let borrow = c.borrow();
            match borrow.as_ref() {
                Some(shared) => shared.heap.is_heap_addr(jobj as usize),
                None => {
                    tracing::warn!(
                        "jobject_to_obj: local ref {jobj:#x} resolved outside JNI context"
                    );
                    None
                }
            }
        })
    }
}

/// Old-style sentinel for use in JniLocalFrame (kept for compatibility).
pub struct JniLocalFrame {
    pub refs: Vec<ObjectRef>,
}

impl JniLocalFrame {
    pub fn new() -> Self {
        Self {
            refs: Vec::with_capacity(16),
        }
    }
    pub fn add(&mut self, obj: ObjectRef) -> JObject {
        self.refs.push(obj);
        obj_to_jobject(obj)
    }
    pub fn remove(&mut self, obj: JObject) {
        if let Some(oref) = jobject_to_obj(obj) {
            self.refs.retain(|r| *r != oref);
        }
    }
}

impl Default for JniLocalFrame {
    fn default() -> Self {
        Self::new()
    }
}

/// Maximum length for C strings received from native code.
const MAX_JNI_CSTR_LEN: usize = 65536;

/// Convert a C string pointer to a Rust `&str`, with length bounds checking.
///
/// # Safety
/// `ptr` must point to a valid null-terminated C string (or be null).
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // Scan up to MAX_JNI_CSTR_LEN bytes to avoid unbounded reads on malformed input.
    let mut len = 0;
    while len < MAX_JNI_CSTR_LEN {
        if *ptr.add(len) == 0 {
            let slice = std::slice::from_raw_parts(ptr as *const u8, len);
            return std::str::from_utf8(slice).ok();
        }
        len += 1;
    }
    tracing::warn!("JNI cstr_to_str: string exceeds {MAX_JNI_CSTR_LEN} byte limit");
    None
}

/// Encode a method identity as a JMethodID.
/// We pack (class_id, method_index) into a u64.
fn encode_method_id(class_id: ClassId, method_index: u16) -> JMethodID {
    ((class_id.as_u32() as u64) << 32) | (method_index as u64)
}

/// Decode a JMethodID back to (class_id, method_index).
fn decode_method_id(mid: JMethodID) -> (ClassId, u16) {
    let class_id = ClassId::new((mid >> 32) as u32);
    let method_index = (mid & 0xFFFF) as u16;
    (class_id, method_index)
}

/// Encode a field identity as a JFieldID.
/// We pack (class_id, field_index) into a u64.
fn encode_field_id(class_id: ClassId, field_index: usize) -> JFieldID {
    ((class_id.as_u32() as u64) << 32) | (field_index as u64)
}

/// Decode a JFieldID back to (class_id, field_index).
fn decode_field_id(fid: JFieldID) -> (ClassId, usize) {
    let class_id = ClassId::new((fid >> 32) as u32);
    let field_index = (fid & 0xFFFF_FFFF) as usize;
    (class_id, field_index)
}

// ---------------------------------------------------------------------------
// JNI function implementations (extern "C")
// ---------------------------------------------------------------------------

// ---- Index 4: GetVersion ----
extern "C" fn jni_get_version(_env: JNIEnv) -> JInt {
    JNI_VERSION_1_8
}

// ---- Index 6: FindClass ----
extern "C" fn jni_find_class(_env: JNIEnv, name: *const c_char) -> JClass {
    let name_str = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => return 0,
    };
    with_shared_vm(|shared| {
        let class_id = shared.load_class_concurrent(name_str).ok()?;
        Some(class_id.as_u32() as JClass)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 10: GetSuperclass ----
extern "C" fn jni_get_superclass(_env: JNIEnv, clazz: JClass) -> JClass {
    if clazz == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let class_id = ClassId::new(clazz as u32);
        let cm = shared.class_manager.read();
        let class = cm.get_class(class_id)?;
        class.superclass.map(|sc| sc.as_u32() as JClass)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 11: IsAssignableFrom ----
extern "C" fn jni_is_assignable_from(_env: JNIEnv, sub: JClass, sup: JClass) -> JBoolean {
    if sub == 0 || sup == 0 {
        return JNI_FALSE;
    }
    with_shared_vm(|shared| {
        let sub_id = ClassId::new(sub as u32);
        let sup_id = ClassId::new(sup as u32);
        if sub_id == sup_id {
            return JNI_TRUE;
        }
        let cm = shared.class_manager.read();
        let mut current = sub_id;
        loop {
            let class = match cm.get_class(current) {
                Some(c) => c,
                None => return JNI_FALSE,
            };
            // Check interfaces
            for &iface_id in &class.interfaces {
                if iface_id == sup_id {
                    return JNI_TRUE;
                }
            }
            match class.superclass {
                Some(sc) => {
                    if sc == sup_id {
                        return JNI_TRUE;
                    }
                    current = sc;
                }
                None => return JNI_FALSE,
            }
        }
    })
    .unwrap_or(JNI_FALSE)
}

// ---- Index 13: Throw ----
extern "C" fn jni_throw(_env: JNIEnv, obj: JThrowable) -> JInt {
    if obj == 0 {
        return JNI_ERR;
    }
    JNI_PENDING_EXCEPTION.with(|cell| {
        cell.set(obj as u64);
    });
    JNI_OK
}

// ---- Index 14: ThrowNew ----
extern "C" fn jni_throw_new(_env: JNIEnv, _clazz: JClass, _msg: *const c_char) -> JInt {
    // Store a marker that an exception was requested via ThrowNew.
    // The interpreter will check this on return from native code.
    JNI_PENDING_EXCEPTION.with(|cell| {
        cell.set(u64::MAX); // sentinel for "exception requested"
    });
    JNI_OK
}

// ---- Index 15: ExceptionOccurred ----
extern "C" fn jni_exception_occurred(_env: JNIEnv) -> JThrowable {
    JNI_PENDING_EXCEPTION.with(|cell| cell.get()) as JThrowable
}

// ---- Index 16: ExceptionDescribe ----
extern "C" fn jni_exception_describe(_env: JNIEnv) {}

// ---- Index 17: ExceptionClear ----
extern "C" fn jni_exception_clear(_env: JNIEnv) {
    JNI_PENDING_EXCEPTION.with(|cell| cell.set(0));
}

// ---- Index 18: FatalError ----
extern "C" fn jni_fatal_error(_env: JNIEnv, msg: *const c_char) {
    let s = unsafe { cstr_to_str(msg).unwrap_or("unknown") };
    eprintln!("JNI FatalError: {s}");
    std::process::abort();
}

// ---- Index 20: PushLocalFrame ----
extern "C" fn jni_push_local_frame(_env: JNIEnv, capacity: JInt) -> JInt {
    push_local_frame(capacity.max(0) as usize);
    JNI_OK
}

// ---- Index 21: PopLocalFrame ----
extern "C" fn jni_pop_local_frame(_env: JNIEnv, result: JObject) -> JObject {
    pop_local_frame(result)
}

// ---- Index 22: NewGlobalRef ----
extern "C" fn jni_new_global_ref(_env: JNIEnv, obj: JObject) -> JObject {
    if obj == 0 {
        return 0;
    }
    // Resolve the object whether it's a local ref or another global ref.
    let oref = match jobject_to_obj(obj) {
        Some(r) => r,
        None => return 0,
    };
    with_shared_vm(|shared| {
        let handle = shared.jni_global_refs.lock().add(oref);
        Some(handle)
    })
    .flatten()
    .unwrap_or_else(|| {
        tracing::warn!("JNI NewGlobalRef: failed to create global reference for handle {obj:#x}");
        0
    })
}

// ---- Index 23: DeleteGlobalRef ----
extern "C" fn jni_delete_global_ref(_env: JNIEnv, gref: JObject) {
    if gref == 0 || gref & 1 == 0 {
        return; // not a global ref handle
    }
    with_shared_vm(|shared| {
        shared.jni_global_refs.lock().remove(gref);
    });
}

// ---- Index 24: DeleteLocalRef ----
extern "C" fn jni_delete_local_ref(_env: JNIEnv, lref: JObject) {
    delete_local_ref(lref);
}

// ---- Index 25: IsSameObject ----
extern "C" fn jni_is_same_object(_env: JNIEnv, a: JObject, b: JObject) -> JBoolean {
    // Resolve both sides through the global-ref layer so that comparing a
    // local ref and a global ref to the same object returns true.
    match (jobject_to_obj(a), jobject_to_obj(b)) {
        (None, None) => JNI_TRUE,
        (Some(ra), Some(rb)) if ra == rb => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

// ---- Index 26: NewLocalRef ----
extern "C" fn jni_new_local_ref(_env: JNIEnv, obj: JObject) -> JObject {
    // Resolve to a raw local-ref pointer (unwrap global-ref tag if present).
    match jobject_to_obj(obj) {
        Some(oref) => {
            let lref = obj_to_jobject(oref);
            track_local_ref(lref);
            lref
        }
        None => 0,
    }
}

// ---- Index 27: EnsureLocalCapacity ----
extern "C" fn jni_ensure_local_capacity(_env: JNIEnv, _capacity: JInt) -> JInt {
    JNI_OK
}

// ---- Index 31: GetObjectClass ----
extern "C" fn jni_get_object_class(_env: JNIEnv, obj: JObject) -> JClass {
    if obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let class_id = shared.heap.class_id_of(oref);
        Some(class_id.as_u32() as JClass)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 32: IsInstanceOf ----
extern "C" fn jni_is_instance_of(_env: JNIEnv, obj: JObject, clazz: JClass) -> JBoolean {
    if obj == 0 {
        return JNI_TRUE; // null is instanceof any type per JNI spec
    }
    if clazz == 0 {
        return JNI_FALSE;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let obj_class_id = shared.heap.class_id_of(oref);
        let target_id = ClassId::new(clazz as u32);
        if obj_class_id == target_id {
            return Some(JNI_TRUE);
        }
        // Walk superclass chain
        let cm = shared.class_manager.read();
        let mut current = obj_class_id;
        loop {
            let class = cm.get_class(current)?;
            for &iface_id in &class.interfaces {
                if iface_id == target_id {
                    return Some(JNI_TRUE);
                }
            }
            match class.superclass {
                Some(sc) => {
                    if sc == target_id {
                        return Some(JNI_TRUE);
                    }
                    current = sc;
                }
                None => return Some(JNI_FALSE),
            }
        }
    })
    .flatten()
    .unwrap_or(JNI_FALSE)
}

// ---- Index 33: GetMethodID ----
extern "C" fn jni_get_method_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JMethodID {
    let name_str = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => return 0,
    };
    let sig_str = match unsafe { cstr_to_str(sig) } {
        Some(s) => s,
        None => return 0,
    };
    if clazz == 0 {
        return 0;
    }
    // Round 8 audit fix (CRIT #2): probe the per-VM `LinkResolver` to
    // dedupe the `(class, name, descriptor)` hierarchy walk. JNI
    // `GetMethodID` is hot on any C-extension entry path (Hibernate
    // ByteBuddy proxies, JNI-heavy libraries like SQLite-JDBC, native
    // image bridges) and the same triple is queried thousands of times
    // per cold start. Cache hits avoid the full `find_method_recursive`
    // walk + the per-class linear `methods` position scan.
    with_shared_vm(|shared| {
        use cratonvm_classloading::resolution::ResolvedMember;
        let class_id = ClassId::new(clazz as u32);
        let resolved = {
            let cm = shared.class_manager.read();
            shared.link_resolver.resolve_or_compute(
                class_id,
                name_str,
                sig_str,
                || {
                    let result = find_method_recursive(
                        class_id, name_str, sig_str, &cm.class_store,
                    )
                    .and_then(|(_, declaring)| {
                        let decl = cm.class_store.get(declaring)?;
                        let idx = decl.methods.iter().position(|m| {
                            &*m.name == name_str && &*m.descriptor == sig_str
                        })?;
                        Some((declaring, idx as u32))
                    });
                    let resolved = match result {
                        Some((d, i)) => ResolvedMember::Method {
                            declaring_class_id: d,
                            index: i,
                        },
                        None => ResolvedMember::NotFound,
                    };
                    (
                        cratonvm_types::intern_arc(name_str),
                        cratonvm_types::intern_arc(sig_str),
                        resolved,
                    )
                },
            )
        };
        match resolved {
            ResolvedMember::Method { declaring_class_id, index } => {
                Some(encode_method_id(declaring_class_id, index as u16))
            }
            _ => None,
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Indices 34-63: Call<Type>MethodA (virtual instance dispatch)
// ---------------------------------------------------------------------------
//
// The varargs (`CallObjectMethod`) and va_list (`CallObjectMethodV`) forms
// cannot be implemented portably in safe Rust.  We provide the *A variants
// (array form) for all return types, plus stub no-ops at the varargs slots
// so that the function table is correctly sized.

extern "C" fn jni_call_object_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    match jni_call_instance(obj, mid, args) {
        Some(Value::Object(Some(r))) => obj_to_jobject(r),
        _ => 0,
    }
}

extern "C" fn jni_call_boolean_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JBoolean,
        _ => 0,
    }
}

extern "C" fn jni_call_byte_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JByte {
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JByte,
        _ => 0,
    }
}

extern "C" fn jni_call_char_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JChar {
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JChar,
        _ => 0,
    }
}

extern "C" fn jni_call_short_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JShort {
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JShort,
        _ => 0,
    }
}

extern "C" fn jni_call_int_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JInt {
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

extern "C" fn jni_call_long_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JLong {
    match jni_call_instance(obj, mid, args) {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as JLong,
        _ => 0,
    }
}

extern "C" fn jni_call_float_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JFloat {
    match jni_call_instance(obj, mid, args) {
        Some(Value::Float(v)) => v,
        _ => 0.0,
    }
}

extern "C" fn jni_call_double_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JDouble {
    match jni_call_instance(obj, mid, args) {
        Some(Value::Double(v)) => v,
        Some(Value::Float(v)) => v as JDouble,
        _ => 0.0,
    }
}

extern "C" fn jni_call_void_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) {
    jni_call_instance(obj, mid, args);
}

// ---------------------------------------------------------------------------
// Indices 64-93: CallNonvirtual<Type>MethodA
// ---------------------------------------------------------------------------

extern "C" fn jni_call_nonvirtual_object_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Object(Some(r))) => obj_to_jobject(r),
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_boolean_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JBoolean,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_byte_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JByte {
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JByte,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_char_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JChar {
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JChar,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_short_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JShort {
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JShort,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_int_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JInt {
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_long_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JLong {
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as JLong,
        _ => 0,
    }
}

extern "C" fn jni_call_nonvirtual_float_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JFloat {
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Float(v)) => v,
        _ => 0.0,
    }
}

extern "C" fn jni_call_nonvirtual_double_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JDouble {
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Double(v)) => v,
        Some(Value::Float(v)) => v as JDouble,
        _ => 0.0,
    }
}

extern "C" fn jni_call_nonvirtual_void_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) {
    jni_call_nonvirtual(obj, clazz, mid, args);
}

// ---------------------------------------------------------------------------
// Indices 114-143: CallStatic<Type>MethodA
// ---------------------------------------------------------------------------

extern "C" fn jni_call_static_object_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    match jni_call_static(clazz, mid, args) {
        Some(Value::Object(Some(r))) => obj_to_jobject(r),
        _ => 0,
    }
}

extern "C" fn jni_call_static_boolean_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JBoolean,
        _ => 0,
    }
}

extern "C" fn jni_call_static_byte_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JByte {
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JByte,
        _ => 0,
    }
}

extern "C" fn jni_call_static_char_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JChar {
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JChar,
        _ => 0,
    }
}

extern "C" fn jni_call_static_short_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JShort {
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JShort,
        _ => 0,
    }
}

extern "C" fn jni_call_static_int_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JInt {
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

extern "C" fn jni_call_static_long_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JLong {
    match jni_call_static(clazz, mid, args) {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as JLong,
        _ => 0,
    }
}

extern "C" fn jni_call_static_float_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JFloat {
    match jni_call_static(clazz, mid, args) {
        Some(Value::Float(v)) => v,
        _ => 0.0,
    }
}

extern "C" fn jni_call_static_double_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JDouble {
    match jni_call_static(clazz, mid, args) {
        Some(Value::Double(v)) => v,
        Some(Value::Float(v)) => v as JDouble,
        _ => 0.0,
    }
}

extern "C" fn jni_call_static_void_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) {
    jni_call_static(clazz, mid, args);
}

// ---- Index 94: GetFieldID ----
extern "C" fn jni_get_field_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JFieldID {
    let name_str = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => return 0,
    };
    // Round 9 audit fix (HIGH #5): include the JNI-supplied signature in
    // the LinkResolver cache key. The previous version dropped the
    // signature and keyed only on `(class_id, name)`; for a class that
    // shadows an inherited field with a different *type* (legal in JVMS
    // §5.4.3.2 — fields are uniquely identified by `(name, descriptor)`),
    // the first JNI `GetFieldID` query would populate the cache with one
    // resolution and every subsequent call — even one passing a
    // different signature — would short-circuit to that wrong entry.
    //
    // Tolerate a null signature by mapping it to "" — pre-fix callers
    // could legitimately pass NULL since the parameter was ignored; we
    // keep that behaviour by treating NULL as a distinct ("no signature
    // assertion") cache key.
    let sig_str = unsafe { cstr_to_str(sig) }.unwrap_or("");
    if clazz == 0 {
        return 0;
    }
    // Round 8 audit fix (CRIT #2): probe the per-VM `LinkResolver`.
    with_shared_vm(|shared| {
        use cratonvm_classloading::resolution::ResolvedMember;
        let class_id = ClassId::new(clazz as u32);
        let resolved = {
            let cm = shared.class_manager.read();
            shared.link_resolver.resolve_or_compute(
                class_id,
                name_str,
                sig_str,
                || {
                    let result = find_field_recursive(
                        class_id, name_str, &cm.class_store,
                    );
                    let resolved = match result {
                        Some((field_index, field, declaring)) => {
                            ResolvedMember::Field {
                                declaring_class_id: declaring,
                                absolute_index: field_index as u32,
                                is_static: field
                                    .access_flags
                                    .contains(
                                        cratonvm_reader::class_access_flags::FieldAccessFlags::STATIC,
                                    ),
                            }
                        }
                        None => ResolvedMember::NotFound,
                    };
                    (
                        cratonvm_types::intern_arc(name_str),
                        cratonvm_types::intern_arc(sig_str),
                        resolved,
                    )
                },
            )
        };
        match resolved {
            ResolvedMember::Field { declaring_class_id, absolute_index, .. } => {
                Some(encode_field_id(declaring_class_id, absolute_index as usize))
            }
            _ => None,
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 95: GetObjectField ----
extern "C" fn jni_get_object_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JObject {
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.heap.get_field(oref, field_index) {
            Value::Object(Some(r)) => Some(obj_to_jobject(r)),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 96: GetBooleanField ----
extern "C" fn jni_get_boolean_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JBoolean {
    get_int_field_raw(obj, field_id) as JBoolean
}

// ---- Index 97: GetByteField ----
extern "C" fn jni_get_byte_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JByte {
    get_int_field_raw(obj, field_id) as JByte
}

// ---- Index 98: GetCharField ----
extern "C" fn jni_get_char_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JChar {
    get_int_field_raw(obj, field_id) as JChar
}

// ---- Index 99: GetShortField ----
extern "C" fn jni_get_short_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JShort {
    get_int_field_raw(obj, field_id) as JShort
}

// ---- Index 100: GetIntField ----
extern "C" fn jni_get_int_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JInt {
    get_int_field_raw(obj, field_id)
}

// ---- Index 101: GetLongField ----
extern "C" fn jni_get_long_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JLong {
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.heap.get_field(oref, field_index) {
            Value::Long(l) => Some(l),
            Value::Int(i) => Some(i as JLong),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 102: GetFloatField ----
extern "C" fn jni_get_float_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JFloat {
    if obj == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.heap.get_field(oref, field_index) {
            Value::Float(f) => Some(f),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

// ---- Index 103: GetDoubleField ----
extern "C" fn jni_get_double_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JDouble {
    if obj == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.heap.get_field(oref, field_index) {
            Value::Double(d) => Some(d),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

// ---- Index 104: SetObjectField ----
extern "C" fn jni_set_object_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JObject) {
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        let value = match jobject_to_obj(val) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        };
        shared.heap.set_field(oref, field_index, value);
        // write_barrier fires automatically inside set_field
        Some(())
    });
}

// ---- Index 105: SetBooleanField ----
extern "C" fn jni_set_boolean_field(
    _env: JNIEnv,
    obj: JObject,
    field_id: JFieldID,
    val: JBoolean,
) {
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 106: SetByteField ----
extern "C" fn jni_set_byte_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JByte) {
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 107: SetCharField ----
extern "C" fn jni_set_char_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JChar) {
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 108: SetShortField ----
extern "C" fn jni_set_short_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JShort) {
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 109: SetIntField ----
extern "C" fn jni_set_int_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JInt) {
    set_int_field_raw(obj, field_id, val);
}

// ---- Index 110: SetLongField ----
extern "C" fn jni_set_long_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JLong) {
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        shared
            .heap
            .set_field(oref, field_index, Value::Long(val));
        Some(())
    });
}

// ---- Index 111: SetFloatField ----
extern "C" fn jni_set_float_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JFloat) {
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        shared
            .heap
            .set_field(oref, field_index, Value::Float(val));
        Some(())
    });
}

// ---- Index 112: SetDoubleField ----
extern "C" fn jni_set_double_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JDouble) {
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        shared
            .heap
            .set_field(oref, field_index, Value::Double(val));
        Some(())
    });
}

// ---- Index 113: GetStaticMethodID ----
extern "C" fn jni_get_static_method_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JMethodID {
    // Same resolution as instance methods — static dispatch is by method index.
    jni_get_method_id(_env, clazz, name, sig)
}

// ---- Index 144: GetStaticFieldID ----
extern "C" fn jni_get_static_field_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JFieldID {
    jni_get_field_id(_env, clazz, name, sig)
}

// ---- Indices 145-150: GetStatic*Field ----
extern "C" fn jni_get_static_object_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JObject {
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Object(Some(r))) => Some(obj_to_jobject(*r)),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

extern "C" fn jni_get_static_boolean_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JBoolean {
    get_static_int_raw(clazz, field_id) as JBoolean
}

extern "C" fn jni_get_static_byte_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JByte {
    get_static_int_raw(clazz, field_id) as JByte
}

extern "C" fn jni_get_static_char_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JChar {
    get_static_int_raw(clazz, field_id) as JChar
}

extern "C" fn jni_get_static_short_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JShort {
    get_static_int_raw(clazz, field_id) as JShort
}

extern "C" fn jni_get_static_int_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JInt {
    get_static_int_raw(clazz, field_id)
}

extern "C" fn jni_get_static_long_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JLong {
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Long(l)) => Some(*l),
            Some(Value::Int(i)) => Some(*i as JLong),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

extern "C" fn jni_get_static_float_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JFloat {
    if clazz == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Float(f)) => Some(*f),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

extern "C" fn jni_get_static_double_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JDouble {
    if clazz == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Double(d)) => Some(*d),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

// ---- Indices 154-159: SetStatic*Field ----
extern "C" fn jni_set_static_object_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JObject,
) {
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let value = match jobject_to_obj(val) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        };
        let mut statics = shared.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields[field_index] = value;
            }
        }
    });
}

extern "C" fn jni_set_static_int_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JInt,
) {
    set_static_int_raw(field_id, val);
}

extern "C" fn jni_set_static_long_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JLong,
) {
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let mut statics = shared.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields[field_index] = Value::Long(val);
            }
        }
    });
}

extern "C" fn jni_set_static_float_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JFloat,
) {
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let mut statics = shared.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields[field_index] = Value::Float(val);
            }
        }
    });
}

extern "C" fn jni_set_static_double_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JDouble,
) {
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let mut statics = shared.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields[field_index] = Value::Double(val);
            }
        }
    });
}

// ---- Index 167: NewStringUTF ----
extern "C" fn jni_new_string_utf(_env: JNIEnv, chars: *const c_char) -> JString {
    let s = match unsafe { cstr_to_str(chars) } {
        Some(s) => s,
        None => return 0,
    };
    with_shared_vm(|shared| {
        let obj = create_java_string(shared, s);
        obj_to_jobject(obj)
    })
    .unwrap_or(0)
}

// ---- Index 168: GetStringUTFLength ----
extern "C" fn jni_get_string_utf_length(_env: JNIEnv, str_obj: JString) -> JSize {
    if str_obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.heap, oref)?;
        Some(s.len() as JSize)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 169: GetStringUTFChars ----
extern "C" fn jni_get_string_utf_chars(
    _env: JNIEnv,
    str_obj: JString,
    is_copy: *mut JBoolean,
) -> *const c_char {
    if str_obj == 0 {
        return std::ptr::null();
    }
    let result = with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.heap, oref)?;
        // Allocate a C string (caller must free with ReleaseStringUTFChars)
        let c_string = std::ffi::CString::new(s).ok()?;
        Some(c_string.into_raw() as *const c_char)
    })
    .flatten();
    match result {
        Some(ptr) => {
            if !is_copy.is_null() {
                unsafe {
                    *is_copy = JNI_TRUE;
                }
            }
            ptr
        }
        None => std::ptr::null(),
    }
}

// ---- Index 170: ReleaseStringUTFChars ----
extern "C" fn jni_release_string_utf_chars(
    _env: JNIEnv,
    _str: JString,
    chars: *const c_char,
) {
    if !chars.is_null() {
        // Free the CString allocated by GetStringUTFChars
        unsafe {
            drop(std::ffi::CString::from_raw(chars as *mut c_char));
        }
    }
}

// ---- Index 171: GetArrayLength ----
extern "C" fn jni_get_array_length(_env: JNIEnv, array: JArray) -> JSize {
    if array == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        Some(shared.heap.array_length(oref) as JSize)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 172: NewObjectArray ----
extern "C" fn jni_new_object_array(
    _env: JNIEnv,
    length: JSize,
    clazz: JClass,
    init: JObject,
) -> JArray {
    if length < 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let component_id = ClassId::new(clazz as u32);
        let arr = shared.heap.alloc_array(
            component_id,
            ArrayElementType::Reference,
            length as usize,
        );
        // Initialize elements if init is non-null
        if init != 0 {
            if let Some(init_ref) = jobject_to_obj(init) {
                for i in 0..length as usize {
                    let _ = shared.heap.set_array_element(
                        arr,
                        i,
                        Value::Object(Some(init_ref)),
                    );
                }
            }
        }
        obj_to_jobject(arr)
    })
    .unwrap_or(0)
}

// ---- Index 173: GetObjectArrayElement ----
extern "C" fn jni_get_object_array_element(
    _env: JNIEnv,
    array: JArray,
    index: JSize,
) -> JObject {
    if array == 0 || index < 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        match shared.heap.get_array_element(oref, index as usize) {
            Ok(Value::Object(Some(r))) => Some(obj_to_jobject(r)),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 174: SetObjectArrayElement ----
extern "C" fn jni_set_object_array_element(
    _env: JNIEnv,
    array: JArray,
    index: JSize,
    val: JObject,
) {
    if array == 0 || index < 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        let value = match jobject_to_obj(val) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        };
        let _ = shared.heap.set_array_element(oref, index as usize, value);
        Some(())
    });
}

// ---- Indices 175-181: New<Type>Array ----
macro_rules! new_prim_array {
    ($name:ident, $elem_type:expr) => {
        extern "C" fn $name(_env: JNIEnv, length: JSize) -> JArray {
            if length < 0 {
                return 0;
            }
            with_shared_vm(|shared| {
                let arr = shared.heap.alloc_array(
                    ClassId::new(0),
                    $elem_type,
                    length as usize,
                );
                obj_to_jobject(arr)
            })
            .unwrap_or(0)
        }
    };
}

new_prim_array!(jni_new_boolean_array, ArrayElementType::Boolean); // 175
new_prim_array!(jni_new_byte_array, ArrayElementType::Byte); // 176
new_prim_array!(jni_new_char_array, ArrayElementType::Char); // 177
new_prim_array!(jni_new_short_array, ArrayElementType::Short); // 178
new_prim_array!(jni_new_int_array, ArrayElementType::Int); // 179
new_prim_array!(jni_new_long_array, ArrayElementType::Long); // 180
new_prim_array!(jni_new_float_array, ArrayElementType::Float); // 181
new_prim_array!(jni_new_double_array, ArrayElementType::Double); // 182

// ---- Indices 183-190: Get<Type>ArrayElements ----
// Returns a pointer to the raw array data. For simplicity, we allocate a copy.
macro_rules! get_array_elements {
    ($name:ident, $rust_type:ty, $value_variant:ident, $default:expr) => {
        extern "C" fn $name(
            _env: JNIEnv,
            array: JArray,
            is_copy: *mut JBoolean,
        ) -> *mut $rust_type {
            if array == 0 {
                return std::ptr::null_mut();
            }
            let result = with_shared_vm(|shared| {
                let oref = jobject_to_obj(array)?;
                let len = shared.heap.array_length(oref);
                let mut buf: Vec<$rust_type> = Vec::with_capacity(len);
                for i in 0..len {
                    let val = match shared.heap.get_array_element(oref, i) {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let elem = match val {
                        Value::$value_variant(v) => v as $rust_type,
                        _ => $default,
                    };
                    buf.push(elem);
                }
                let ptr = buf.as_mut_ptr();
                std::mem::forget(buf); // OWNERSHIP: buffer transferred to native caller, freed by Release<Type>ArrayElements via Vec::from_raw_parts
                Some(ptr)
            })
            .flatten();
            match result {
                Some(ptr) => {
                    if !is_copy.is_null() {
                        unsafe {
                            *is_copy = JNI_TRUE;
                        }
                    }
                    ptr
                }
                None => std::ptr::null_mut(),
            }
        }
    };
}

get_array_elements!(jni_get_boolean_array_elements, JBoolean, Int, 0); // 183
get_array_elements!(jni_get_byte_array_elements, JByte, Int, 0); // 184
get_array_elements!(jni_get_char_array_elements, JChar, Int, 0); // 185
get_array_elements!(jni_get_short_array_elements, JShort, Int, 0); // 186
get_array_elements!(jni_get_int_array_elements, JInt, Int, 0); // 187
get_array_elements!(jni_get_long_array_elements, JLong, Long, 0); // 188
get_array_elements!(jni_get_float_array_elements, JFloat, Float, 0.0); // 189
get_array_elements!(jni_get_double_array_elements, JDouble, Double, 0.0); // 190

// ---- Indices 191-198: Release<Type>ArrayElements ----
// Frees the buffer allocated by Get<Type>ArrayElements and optionally copies back.
macro_rules! release_array_elements {
    ($name:ident, $rust_type:ty, $value_constructor:expr) => {
        extern "C" fn $name(
            _env: JNIEnv,
            array: JArray,
            elems: *mut $rust_type,
            mode: JInt,
        ) {
            if elems.is_null() {
                return;
            }
            // mode 0 = copy back and free, JNI_COMMIT = copy back don't free,
            // JNI_ABORT = free without copy back
            if mode != 2 && array != 0 {
                // Copy back to array (mode 0 or JNI_COMMIT=1)
                with_shared_vm(|shared| {
                    let oref = jobject_to_obj(array)?;
                    let len = shared.heap.array_length(oref);
                    for i in 0..len {
                        let val = unsafe { *elems.add(i) };
                        let value = $value_constructor(val);
                        let _ = shared.heap.set_array_element(oref, i, value);
                    }
                    Some(())
                });
            }
            if mode != 1 {
                // Free buffer (mode 0 or JNI_ABORT=2)
                // Reconstruct Vec to free — we need the length.
                // Since we don't track length, use array length.
                if array != 0 {
                    let len = with_shared_vm(|shared| {
                        jobject_to_obj(array)
                            .map(|oref| shared.heap.array_length(oref))
                    })
                    .flatten()
                    .unwrap_or(0);
                    if len > 0 {
                        unsafe {
                            drop(Vec::from_raw_parts(elems, len, len));
                        }
                    }
                }
            }
        }
    };
}

release_array_elements!(jni_release_boolean_array_elements, JBoolean, |v: JBoolean| Value::Int(v as i32)); // 191
release_array_elements!(jni_release_byte_array_elements, JByte, |v: JByte| Value::Int(v as i32)); // 192
release_array_elements!(jni_release_char_array_elements, JChar, |v: JChar| Value::Int(v as i32)); // 193
release_array_elements!(jni_release_short_array_elements, JShort, |v: JShort| Value::Int(v as i32)); // 194
release_array_elements!(jni_release_int_array_elements, JInt, |v: JInt| Value::Int(v)); // 195
release_array_elements!(jni_release_long_array_elements, JLong, |v: JLong| Value::Long(v)); // 196
release_array_elements!(jni_release_float_array_elements, JFloat, |v: JFloat| Value::Float(v)); // 197
release_array_elements!(jni_release_double_array_elements, JDouble, |v: JDouble| Value::Double(v)); // 198

// ---- Indices 199-206: Get<Type>ArrayRegion ----
macro_rules! get_array_region {
    ($name:ident, $rust_type:ty, $value_variant:ident, $default:expr) => {
        extern "C" fn $name(
            _env: JNIEnv,
            array: JArray,
            start: JSize,
            len: JSize,
            buf: *mut $rust_type,
        ) {
            if array == 0 || buf.is_null() || start < 0 || len < 0 {
                return;
            }
            with_shared_vm(|shared| {
                let oref = jobject_to_obj(array)?;
                for i in 0..len as usize {
                    let val = shared
                        .heap
                        .get_array_element(oref, start as usize + i)
                        .ok()?;
                    let elem = match val {
                        Value::$value_variant(v) => v as $rust_type,
                        _ => $default,
                    };
                    unsafe {
                        *buf.add(i) = elem;
                    }
                }
                Some(())
            });
        }
    };
}

get_array_region!(jni_get_boolean_array_region, JBoolean, Int, 0); // 199
get_array_region!(jni_get_byte_array_region, JByte, Int, 0); // 200
get_array_region!(jni_get_char_array_region, JChar, Int, 0); // 201
get_array_region!(jni_get_short_array_region, JShort, Int, 0); // 202
get_array_region!(jni_get_int_array_region, JInt, Int, 0); // 203
get_array_region!(jni_get_long_array_region, JLong, Long, 0); // 204
get_array_region!(jni_get_float_array_region, JFloat, Float, 0.0); // 205
get_array_region!(jni_get_double_array_region, JDouble, Double, 0.0); // 206

// ---- Indices 207-214: Set<Type>ArrayRegion ----
macro_rules! set_array_region {
    ($name:ident, $rust_type:ty, $value_constructor:expr) => {
        extern "C" fn $name(
            _env: JNIEnv,
            array: JArray,
            start: JSize,
            len: JSize,
            buf: *const $rust_type,
        ) {
            if array == 0 || buf.is_null() || start < 0 || len < 0 {
                return;
            }
            with_shared_vm(|shared| {
                let oref = jobject_to_obj(array)?;
                for i in 0..len as usize {
                    let val = unsafe { *buf.add(i) };
                    let _ = shared.heap.set_array_element(
                        oref,
                        start as usize + i,
                        $value_constructor(val),
                    );
                }
                Some(())
            });
        }
    };
}

set_array_region!(jni_set_boolean_array_region, JBoolean, |v: JBoolean| Value::Int(v as i32)); // 207
set_array_region!(jni_set_byte_array_region, JByte, |v: JByte| Value::Int(v as i32)); // 208
set_array_region!(jni_set_char_array_region, JChar, |v: JChar| Value::Int(v as i32)); // 209
set_array_region!(jni_set_short_array_region, JShort, |v: JShort| Value::Int(v as i32)); // 210
set_array_region!(jni_set_int_array_region, JInt, |v: JInt| Value::Int(v)); // 211
set_array_region!(jni_set_long_array_region, JLong, |v: JLong| Value::Long(v)); // 212
set_array_region!(jni_set_float_array_region, JFloat, |v: JFloat| Value::Float(v)); // 213
set_array_region!(jni_set_double_array_region, JDouble, |v: JDouble| Value::Double(v)); // 214

// ---- Index 217: MonitorEnter ----
extern "C" fn jni_monitor_enter(_env: JNIEnv, obj: JObject) -> JInt {
    if obj == 0 {
        return JNI_ERR;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        shared.monitors.enter(oref, ThreadId(0)); // thread_id 0 for JNI
        Some(JNI_OK)
    })
    .flatten()
    .unwrap_or(JNI_ERR)
}

// ---- Index 218: MonitorExit ----
extern "C" fn jni_monitor_exit(_env: JNIEnv, obj: JObject) -> JInt {
    if obj == 0 {
        return JNI_ERR;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let _ = shared.monitors.exit(oref, ThreadId(0));
        Some(JNI_OK)
    })
    .flatten()
    .unwrap_or(JNI_ERR)
}

// ---- Index 219: GetJavaVM ----
extern "C" fn jni_get_java_vm(_env: JNIEnv, vm: *mut JavaVM) -> JInt {
    if vm.is_null() {
        return JNI_ERR;
    }
    init_jni_table();
    // `AtomicPtr<usize>` has the same layout as `*mut usize`, so the address of
    // the atomic itself is a valid pointer to the invoke-table pointer.
    unsafe {
        *vm = std::ptr::addr_of!(JNI_INVOKE_TABLE_PTR).cast();
    }
    JNI_OK
}

// ---- Index 228: ExceptionCheck ----
extern "C" fn jni_exception_check(_env: JNIEnv) -> JBoolean {
    JNI_FALSE
}

// ---- Index 5: DefineClass ----
extern "C" fn jni_define_class(
    _env: JNIEnv,
    name: *const c_char,
    _loader: JObject,
    _buf: *const u8,
    _len: JSize,
) -> JClass {
    // DefineClass from raw bytes: parse the name and load the class via the
    // standard class manager path. Full bytecode injection is not yet supported.
    let class_name = match unsafe { cstr_to_str(name) } {
        Some(s) => s.replace('.', "/"),
        None => return 0,
    };
    with_shared_vm(|shared| {
        let mut cm = shared.class_manager.write();
        let class_id = cm.load_class(&class_name).ok()?;
        Some(class_id.as_u32() as JClass)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 7: FromReflectedMethod ----
// Convert a java.lang.reflect.Method/Constructor to a JMethodID.
extern "C" fn jni_from_reflected_method(_env: JNIEnv, method: JObject) -> JMethodID {
    if method == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(method)?;
        // Reflected method objects store class_id and method_index in fields 0 and 1.
        let class_id_val = match shared.heap.get_field(oref, 0) {
            Value::Int(i) => i as u32,
            _ => return None,
        };
        let method_idx = match shared.heap.get_field(oref, 1) {
            Value::Int(i) => i as u16,
            _ => return None,
        };
        Some(encode_method_id(ClassId::new(class_id_val), method_idx))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 8: FromReflectedField ----
// Convert a java.lang.reflect.Field to a JFieldID.
extern "C" fn jni_from_reflected_field(_env: JNIEnv, field: JObject) -> JFieldID {
    if field == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(field)?;
        // Reflected field objects store class_id in field 0 and field_index in field 1.
        let class_id_val = match shared.heap.get_field(oref, 0) {
            Value::Int(i) => i as u32,
            _ => return None,
        };
        let field_idx = match shared.heap.get_field(oref, 1) {
            Value::Int(i) => i as usize,
            _ => return None,
        };
        Some(encode_field_id(ClassId::new(class_id_val), field_idx))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 9: ToReflectedMethod ----
// Convert a JMethodID to a java.lang.reflect.Method object.
extern "C" fn jni_to_reflected_method(
    _env: JNIEnv,
    clazz: JClass,
    method_id: JMethodID,
    _is_static: JBoolean,
) -> JObject {
    if method_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, method_index) = decode_method_id(method_id);
        let class_id = if clazz != 0 {
            ClassId::new(clazz as u32)
        } else {
            decl_class_id
        };
        // Allocate a synthetic Method object with class_id and method_index in fields.
        // Resolve the real `java.lang.reflect.Method` class — `find_class_by_name`
        // only sees already-loaded classes, so `load_class_concurrent` is used to
        // force the load. Allocating with `ClassId::new(0)` (`java/lang/Object`,
        // zero declared fields) but 4 slots produces an undersized object the
        // GC's `get_field` bounds guard rejects.
        let method_class_id = shared
            .load_class_concurrent("java/lang/reflect/Method")
            .unwrap_or_else(|_| {
                shared
                    .class_manager
                    .write()
                    .ensure_synthetic_class("java/lang/reflect/Method", 4)
            });
        let num_fields = shared
            .class_manager
            .read()
            .get_class(method_class_id)
            .map_or(4, |c| c.num_total_fields.max(4));
        let obj = shared.heap.alloc_object(method_class_id, num_fields);
        shared.heap.set_field(obj, 0, Value::Int(class_id.as_u32() as i32));
        shared.heap.set_field(obj, 1, Value::Int(method_index as i32));
        obj_to_jobject(obj)
    })
    .unwrap_or(0)
}

// ---- Index 12: ToReflectedField ----
// Convert a JFieldID to a java.lang.reflect.Field object.
extern "C" fn jni_to_reflected_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
    _is_static: JBoolean,
) -> JObject {
    if field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let class_id = if clazz != 0 {
            ClassId::new(clazz as u32)
        } else {
            decl_class_id
        };
        // Resolve the real `java.lang.reflect.Field` class (see the matching
        // comment in `jni_to_reflected_method`): allocating with
        // `ClassId::new(0)` + 4 slots produces an undersized object the GC's
        // `get_field` bounds guard rejects.
        let field_class_id = shared
            .load_class_concurrent("java/lang/reflect/Field")
            .unwrap_or_else(|_| {
                shared
                    .class_manager
                    .write()
                    .ensure_synthetic_class("java/lang/reflect/Field", 4)
            });
        let num_fields = shared
            .class_manager
            .read()
            .get_class(field_class_id)
            .map_or(4, |c| c.num_total_fields.max(4));
        let obj = shared.heap.alloc_object(field_class_id, num_fields);
        shared.heap.set_field(obj, 0, Value::Int(class_id.as_u32() as i32));
        shared.heap.set_field(obj, 1, Value::Int(field_index as i32));
        obj_to_jobject(obj)
    })
    .unwrap_or(0)
}

// ---- Index 220: GetStringRegion ----
extern "C" fn jni_get_string_region(
    _env: JNIEnv,
    str_obj: JString,
    start: JSize,
    len: JSize,
    buf: *mut JChar,
) {
    if str_obj == 0 || buf.is_null() || start < 0 || len < 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.heap, oref)?;
        let utf16: Vec<u16> = s.encode_utf16().collect();
        let start = start as usize;
        let len = len as usize;
        if start + len > utf16.len() {
            return None;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                utf16[start..start + len].as_ptr(),
                buf,
                len,
            );
        }
        Some(())
    });
}

// ---- Index 221: GetStringUTFRegion ----
extern "C" fn jni_get_string_utf_region(
    _env: JNIEnv,
    str_obj: JString,
    start: JSize,
    len: JSize,
    buf: *mut c_char,
) {
    if str_obj == 0 || buf.is_null() || start < 0 || len < 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.heap, oref)?;
        // In JNI, start/len refer to UTF-16 code units.
        let utf16: Vec<u16> = s.encode_utf16().collect();
        let start = start as usize;
        let len = len as usize;
        if start + len > utf16.len() {
            return None;
        }
        let region = String::from_utf16_lossy(&utf16[start..start + len]);
        let bytes = region.as_bytes();
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, bytes.len());
            // Null-terminate
            *buf.add(bytes.len()) = 0;
        }
        Some(())
    });
}

/// Metadata for a temporary contiguous buffer handed out by
/// `GetPrimitiveArrayCritical` for a G1 humongous array. See
/// `JNI_CRITICAL_COPIES`.
struct CriticalCopy {
    /// The originating array handle, so release can copy back / free.
    array: JArray,
    /// Element type, so release re-packs each element correctly.
    element_type: ArrayElementType,
    /// Number of elements (== array length at Get time).
    len: usize,
    /// Bytes per element (the contiguous buffer is `len * stride` bytes).
    stride: usize,
}

/// Bytes per stored element for a primitive array element type.
fn critical_stride(et: ArrayElementType) -> usize {
    match et {
        ArrayElementType::Byte | ArrayElementType::Boolean => 1,
        ArrayElementType::Char | ArrayElementType::Short => 2,
        ArrayElementType::Int | ArrayElementType::Float => 4,
        ArrayElementType::Long | ArrayElementType::Double => 8,
        // Reference arrays are not valid for the primitive-critical API.
        ArrayElementType::Reference => 0,
    }
}

/// Write the host-endian bytes of array element `i` (read via the
/// region-safe accessor) into `dst[i*stride .. (i+1)*stride]`.
fn critical_encode_element(v: Value, et: ArrayElementType, dst: &mut [u8]) {
    match et {
        ArrayElementType::Byte | ArrayElementType::Boolean => {
            let x = v.as_int().unwrap_or(0) as i8;
            dst[0] = x as u8;
        }
        ArrayElementType::Short => {
            let x = v.as_int().unwrap_or(0) as i16;
            dst[..2].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Char => {
            let x = v.as_int().unwrap_or(0) as u16;
            dst[..2].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Int => {
            let x = v.as_int().unwrap_or(0);
            dst[..4].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Float => {
            let x = match v {
                Value::Float(f) => f,
                _ => 0.0,
            };
            dst[..4].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Long => {
            let x = match v {
                Value::Long(l) => l,
                _ => 0,
            };
            dst[..8].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Double => {
            let x = match v {
                Value::Double(d) => d,
                _ => 0.0,
            };
            dst[..8].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Reference => {}
    }
}

/// Decode element `i` from `src[i*stride .. (i+1)*stride]` (host-endian)
/// back into a `Value` for store via the region-safe accessor.
fn critical_decode_element(et: ArrayElementType, src: &[u8]) -> Value {
    match et {
        ArrayElementType::Byte | ArrayElementType::Boolean => {
            Value::Int(src[0] as i8 as i32)
        }
        ArrayElementType::Short => {
            let x = i16::from_ne_bytes([src[0], src[1]]);
            Value::Int(x as i32)
        }
        ArrayElementType::Char => {
            let x = u16::from_ne_bytes([src[0], src[1]]);
            Value::Int(x as i32)
        }
        ArrayElementType::Int => {
            Value::Int(i32::from_ne_bytes([src[0], src[1], src[2], src[3]]))
        }
        ArrayElementType::Float => {
            Value::Float(f32::from_ne_bytes([src[0], src[1], src[2], src[3]]))
        }
        ArrayElementType::Long => Value::Long(i64::from_ne_bytes([
            src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
        ])),
        ArrayElementType::Double => Value::Double(f64::from_ne_bytes([
            src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
        ])),
        ArrayElementType::Reference => Value::Object(None),
    }
}

// ---- Index 222: GetPrimitiveArrayCritical ----
// Returns a direct pointer to the array data (no copy if possible).
extern "C" fn jni_get_primitive_array_critical(
    _env: JNIEnv,
    array: JArray,
    is_copy: *mut JBoolean,
) -> *mut std::ffi::c_void {
    if array == 0 {
        return std::ptr::null_mut();
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        match shared.heap.array_data_ptr(oref) {
            // Ordinary (single-region) array: hand out the live, contiguous
            // payload pointer — no copy, release is a no-op.
            Some(ptr) => {
                if !is_copy.is_null() {
                    unsafe { *is_copy = JNI_FALSE; } // Direct pointer, no copy
                }
                Some(ptr as *mut std::ffi::c_void)
            }
            // G1 humongous array: the payload spans non-contiguous regions, so
            // the JNI contract's "direct pointer" cannot be honoured. Fall
            // back to the same copy-out / copy-back scheme the non-critical
            // `Get<Type>ArrayElements` path uses: materialise a contiguous
            // buffer via the region-safe accessor, hand it out, and register
            // it so `ReleasePrimitiveArrayCritical` copies any mutations back
            // and frees it.
            None => {
                let element_type = shared.heap.array_element_type(oref)?;
                let stride = critical_stride(element_type);
                if stride == 0 {
                    return None; // reference array — not a primitive critical
                }
                let len = shared.heap.array_length(oref);
                let mut buf: Vec<u8> = vec![0u8; len * stride];
                for i in 0..len {
                    let v = match shared.heap.get_array_element(oref, i) {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let off = i * stride;
                    critical_encode_element(v, element_type, &mut buf[off..off + stride]);
                }
                let ptr = buf.as_mut_ptr();
                std::mem::forget(buf); // OWNERSHIP: transferred to native caller, reclaimed by jni_release_primitive_array_critical
                JNI_CRITICAL_COPIES.with(|c| {
                    c.borrow_mut().insert(
                        ptr as usize,
                        CriticalCopy { array, element_type, len, stride },
                    );
                });
                if !is_copy.is_null() {
                    unsafe { *is_copy = JNI_TRUE; } // Copy, not a direct pointer
                }
                Some(ptr as *mut std::ffi::c_void)
            }
        }
    })
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

// ---- Index 223: ReleasePrimitiveArrayCritical ----
extern "C" fn jni_release_primitive_array_critical(
    _env: JNIEnv,
    _array: JArray,
    carray: *mut std::ffi::c_void,
    mode: JInt,
) {
    if carray.is_null() {
        return;
    }
    // Fast path: for ordinary arrays we handed out a direct pointer and
    // recorded nothing, so there is nothing to copy back or free.
    let copy = JNI_CRITICAL_COPIES.with(|c| c.borrow_mut().remove(&(carray as usize)));
    let Some(copy) = copy else {
        return;
    };
    // Humongous fallback buffer. mode 0 = copy back + free, JNI_COMMIT (1) =
    // copy back, don't free, JNI_ABORT (2) = free without copy back.
    if mode != 2 {
        with_shared_vm(|shared| {
            let oref = jobject_to_obj(copy.array)?;
            for i in 0..copy.len {
                let off = i * copy.stride;
                // SAFETY: the buffer is `copy.len * copy.stride` bytes and we
                // only read within it.
                let src = unsafe {
                    std::slice::from_raw_parts(
                        (carray as *const u8).add(off),
                        copy.stride,
                    )
                };
                let v = critical_decode_element(copy.element_type, src);
                let _ = shared.heap.set_array_element(oref, i, v);
            }
            Some(())
        });
    }
    if mode != 1 {
        // Reconstruct the `Vec<u8>` with its original layout and drop it.
        let byte_len = copy.len * copy.stride;
        // SAFETY: `carray` was produced by `Vec::<u8>::as_mut_ptr` +
        // `mem::forget` in `jni_get_primitive_array_critical` with capacity
        // == length == `byte_len`; we reconstruct the exact layout.
        unsafe {
            drop(Vec::from_raw_parts(carray as *mut u8, byte_len, byte_len));
        }
    } else {
        // JNI_COMMIT: we kept the buffer alive but already removed it from the
        // map; re-insert so a later release can still find it.
        JNI_CRITICAL_COPIES.with(|c| {
            c.borrow_mut().insert(carray as usize, copy);
        });
    }
}

// ---- Index 224: GetStringCritical ----
// Same as GetStringChars but signals the VM not to move the string.
extern "C" fn jni_get_string_critical(
    _env: JNIEnv,
    str_obj: JString,
    is_copy: *mut JBoolean,
) -> *const JChar {
    // Delegate to GetStringChars — our heap doesn't move objects between GC.
    jni_get_string_chars(_env, str_obj, is_copy)
}

// ---- Index 225: ReleaseStringCritical ----
extern "C" fn jni_release_string_critical(
    _env: JNIEnv,
    str_obj: JString,
    chars: *const JChar,
) {
    jni_release_string_chars(_env, str_obj, chars);
}

// ---- Index 226: NewWeakGlobalRef ----
// Weak refs behave like global refs but don't prevent GC. For simplicity,
// we implement them as strong global refs (conservative but correct).
extern "C" fn jni_new_weak_global_ref(_env: JNIEnv, obj: JObject) -> JObject {
    if obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let mut refs = shared.jni_global_refs.lock();
        Some(refs.add(oref))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 227: DeleteWeakGlobalRef ----
extern "C" fn jni_delete_weak_global_ref(_env: JNIEnv, wref: JObject) {
    if wref == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let mut refs = shared.jni_global_refs.lock();
        refs.remove(wref);
        Some(())
    });
}

// ---- Index 19: GetObjectRefType ----
// Returns the type of the reference: JNIInvalidRefType=0, JNILocalRefType=1,
// JNIGlobalRefType=2, JNIWeakGlobalRefType=3.
extern "C" fn jni_get_object_ref_type(_env: JNIEnv, obj: JObject) -> JInt {
    if obj == 0 {
        return 0; // JNIInvalidRefType
    }
    if obj & 1 == 1 {
        2 // JNIGlobalRefType (global or weak — we treat weak as global)
    } else {
        1 // JNILocalRefType
    }
}

/// Stub for unimplemented JNI functions. Logs a warning and returns 0.
extern "C" fn jni_stub() -> usize {
    tracing::warn!("unimplemented JNI function called");
    0
}

// ---------------------------------------------------------------------------
// Internal helpers for field access
// ---------------------------------------------------------------------------

fn get_int_field_raw(obj: JObject, field_id: JFieldID) -> JInt {
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.heap.get_field(oref, field_index) {
            Value::Int(i) => Some(i),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

fn set_int_field_raw(obj: JObject, field_id: JFieldID, val: i32) {
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        shared
            .heap
            .set_field(oref, field_index, Value::Int(val));
        Some(())
    });
}

fn get_static_int_raw(clazz: JClass, field_id: JFieldID) -> JInt {
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.get(field_index) {
            Some(Value::Int(i)) => Some(*i),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

fn set_static_int_raw(field_id: JFieldID, val: JInt) {
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let mut statics = shared.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields[field_index] = Value::Int(val);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// JNI Native Method Registry (RegisterNatives)
// ---------------------------------------------------------------------------

/// C-compatible struct matching `JNINativeMethod` in jni.h.
/// Used by `RegisterNatives` to pass (name, signature, function pointer) triples.
#[repr(C)]
struct JNINativeMethod {
    name: *const c_char,
    signature: *const c_char,
    fn_ptr: *mut (),
}

// Safety: the pointers in JNINativeMethod are only read while holding the
// caller's guarantee that they're valid (they're passed in from C). The struct
// itself is never stored.

/// Global table of JNI native function pointers registered via `RegisterNatives`.
/// Key = FNV-1a hash of "class_name.method_nameDescriptor".
/// Value = raw function pointer (to be called with `dispatch_jni_native`).
static JNI_NATIVE_METHODS: std::sync::LazyLock<parking_lot::RwLock<HashMap<u64, usize>>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(HashMap::new()));

/// Compute a hash key for a (class, method, descriptor) triple.
/// Identical algorithm to `NativeMethodRegistry::native_method_hash`.
fn jni_native_key(class_name: &str, method_name: &str, descriptor: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in class_name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= b'.' as u64;
    h = h.wrapping_mul(0x100000001b3);
    for b in method_name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= b'.' as u64;
    h = h.wrapping_mul(0x100000001b3);
    for b in descriptor.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Store a JNI function pointer registered via `RegisterNatives` or symbol lookup.
pub fn register_jni_native(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    fn_ptr: usize,
) {
    let key = jni_native_key(class_name, method_name, descriptor);
    let mut table = JNI_NATIVE_METHODS.write();
    if let Some(&existing) = table.get(&key) {
        if existing != fn_ptr {
            tracing::warn!(
                "JNI native method hash collision or re-registration: {}.{}{} (replacing 0x{:x} with 0x{:x})",
                class_name, method_name, descriptor, existing, fn_ptr
            );
        }
    }
    table.insert(key, fn_ptr);
}

/// Look up a JNI function pointer for the given method.
/// Returns `None` if no pointer was registered.
pub fn find_jni_native(class_name: &str, method_name: &str, descriptor: &str) -> Option<usize> {
    let key = jni_native_key(class_name, method_name, descriptor);
    JNI_NATIVE_METHODS.read().get(&key).copied()
}

// ---------------------------------------------------------------------------
// JNI name mangling (JNI spec §11.3)
// ---------------------------------------------------------------------------

/// Encode a single component (class name or method name) for JNI symbol lookup.
/// JNI encoding rules:
///   `/` → `_`   (package separator)
///   `_` → `_1`  (literal underscore)
///   `;` → `_2`  (descriptor separator)
///   `[` → `_3`  (array prefix)
///   Unicode `\uXXXX` → `_0XXXX`
fn jni_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        match ch {
            '/' => out.push('_'),
            '_' => out.push_str("_1"),
            ';' => out.push_str("_2"),
            '[' => out.push_str("_3"),
            c if c.is_ascii() => out.push(c),
            c => {
                // Unicode escape: _0XXXX
                out.push_str(&format!("_0{:04x}", c as u32));
            }
        }
    }
    out
}

/// Build the JNI short name: `Java_<class>_<method>`.
///
/// Example: `("java/lang/System", "arraycopy")` → `"Java_java_lang_System_arraycopy"`.
pub fn jni_short_name(class_name: &str, method_name: &str) -> String {
    format!("Java_{}_{}", jni_encode(class_name), jni_encode(method_name))
}

/// Build the JNI long name: `Java_<class>_<method>__<encoded_params>`.
///
/// The parameter portion is the substring between `(` and `)` in the descriptor,
/// encoded with the same rules plus `;` → `_2` and `[` → `_3`.
///
/// Example: `("java/lang/System", "arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V")`
///   → `"Java_java_lang_System_arraycopy__Ljava_lang_Object_2ILjava_lang_Object_2II"`.
pub fn jni_long_name(class_name: &str, method_name: &str, descriptor: &str) -> String {
    let params = descriptor
        .strip_prefix('(')
        .and_then(|s| s.split(')').next())
        .unwrap_or("");
    format!(
        "Java_{}_{}__{}",
        jni_encode(class_name),
        jni_encode(method_name),
        jni_encode(params)
    )
}

/// Try to resolve a native method by JNI naming convention in loaded libraries.
///
/// Tries the short name first, then the long name (JNI spec §11.3 resolution order).
/// If found, the function pointer is cached in `JNI_NATIVE_METHODS` for future lookups.
pub fn resolve_jni_native_in_libraries(
    native_libraries: &parking_lot::Mutex<Vec<libloading::Library>>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<usize> {
    let short = jni_short_name(class_name, method_name);
    let long = jni_long_name(class_name, method_name, descriptor);

    let libs = native_libraries.lock();
    for symbol_name in [&short, &long] {
        let c_name = match std::ffi::CString::new(symbol_name.as_bytes()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for lib in libs.iter() {
            // Safety: we are looking up a symbol name that follows JNI conventions.
            if let Ok(sym) = unsafe { lib.get::<*const ()>(c_name.as_bytes_with_nul()) } {
                let fn_ptr = *sym as usize;
                if fn_ptr != 0 {
                    // Cache for future lookups
                    drop(libs);
                    register_jni_native(class_name, method_name, descriptor, fn_ptr);
                    tracing::info!(
                        symbol = %symbol_name,
                        method = %format!("{}.{}{}", class_name, method_name, descriptor),
                        "Auto-resolved native method via JNI naming convention"
                    );
                    return Some(fn_ptr);
                }
            }
        }
    }
    None
}

/// Dispatch a JNI native function pointer call.
///
/// Converts `Value` args to 64-bit C values (correct for all integer/reference
/// types on x86-64). Float/double args are passed as their bit representation
/// in integer registers — this is ABI-correct on Windows x64, but not on
/// Linux x86-64 System V for the float/double parameter positions.
///
/// # Safety
/// `fn_ptr` must be a valid JNI native function whose signature matches
/// `descriptor`, and the TLS JNI context must already be set.
pub unsafe fn dispatch_jni_native(
    fn_ptr: usize,
    env: JNIEnv,
    receiver: JObject,
    args: &[Value],
    descriptor: &str,
) -> Value {
    let param_types = parse_param_types_cached(descriptor);

    #[cfg(not(target_os = "windows"))]
    {
        static FLOAT_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if param_types.iter().any(|&t| t == b'F' || t == b'D')
            && !FLOAT_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                "JNI dispatch with float/double args on non-Windows platform — \
                 values are passed as integer bit patterns which may not match \
                 the System V ABI XMM register convention"
            );
        }
    }

    // Convert each Value to a raw u64. Object refs → pointer as u64,
    // floats/doubles → bit representation. Works for all non-float args on all
    // x86-64 ABIs; float support on Linux x86-64 requires libffi.
    let raw: Vec<u64> = args
        .iter()
        .zip(param_types.iter())
        .map(|(v, _tag)| match v {
            Value::Int(i) => *i as i64 as u64,
            Value::Long(l) => *l as u64,
            Value::Float(f) => f.to_bits() as u64,
            Value::Double(d) => d.to_bits(),
            Value::Object(Some(r)) => obj_to_jobject(*r),
            Value::Object(None) => 0u64,
            _ => 0u64,
        })
        .collect();

    let raw_result = call_jni_fn_ptr(fn_ptr, env, receiver, &raw);

    // Determine return type from descriptor (byte after the closing ')')
    let ret_char = descriptor
        .rfind(')')
        .and_then(|i| descriptor.as_bytes().get(i + 1).copied())
        .unwrap_or(b'V');

    match ret_char {
        b'V' => Value::Object(None),
        b'Z' => Value::Int((raw_result & 1) as i32),
        b'B' => Value::Int(raw_result as i8 as i32),
        b'C' => Value::Int(raw_result as u16 as i32),
        b'S' => Value::Int(raw_result as i16 as i32),
        b'I' => Value::Int(raw_result as i32),
        b'J' => Value::Long(raw_result as i64),
        b'F' => Value::Float(f32::from_bits(raw_result as u32)),
        b'D' => Value::Double(f64::from_bits(raw_result)),
        _ => match jobject_to_obj(raw_result) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        },
    }
}

/// Call a raw `extern "C"` function pointer with (env, recv, args[0..N]).
/// All arguments and the return value are treated as `u64`.
///
/// # Safety
/// `fn_ptr` must be a valid function pointer whose actual C signature is
/// compatible with receiving all arguments as 64-bit integers.
unsafe fn call_jni_fn_ptr(fn_ptr: usize, env: JNIEnv, recv: JObject, args: &[u64]) -> u64 {
    // Validate function pointer before transmute to prevent calling
    // null, misaligned, or obviously-invalid pointers.
    if fn_ptr == 0 {
        tracing::error!("JNI call with null function pointer — returning 0");
        return 0;
    }
    // Alignment check: function pointers must be at least 2-byte aligned
    // on all modern architectures (4-byte on ARM).
    #[cfg(target_arch = "aarch64")]
    if fn_ptr % 4 != 0 {
        tracing::error!("JNI call with misaligned function pointer {:#x} — returning 0", fn_ptr);
        return 0;
    }
    #[cfg(not(target_arch = "aarch64"))]
    if fn_ptr % 2 != 0 {
        tracing::error!("JNI call with misaligned function pointer {:#x} — returning 0", fn_ptr);
        return 0;
    }

    // We use explicit transmutes to extern "C" function types so that Rust
    // generates correct platform-ABI call sequences (register assignments,
    // stack layout) for each argument count.
    let a = args;
    match a.len() {
        0 => {
            let f: extern "C" fn(JNIEnv, JObject) -> u64 = std::mem::transmute(fn_ptr);
            f(env, recv)
        }
        1 => {
            let f: extern "C" fn(JNIEnv, JObject, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(env, recv, a[0])
        }
        2 => {
            let f: extern "C" fn(JNIEnv, JObject, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(env, recv, a[0], a[1])
        }
        3 => {
            let f: extern "C" fn(JNIEnv, JObject, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(env, recv, a[0], a[1], a[2])
        }
        4 => {
            let f: extern "C" fn(JNIEnv, JObject, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(env, recv, a[0], a[1], a[2], a[3])
        }
        5 => {
            let f: extern "C" fn(JNIEnv, JObject, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(env, recv, a[0], a[1], a[2], a[3], a[4])
        }
        6 => {
            let f: extern "C" fn(JNIEnv, JObject, u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(env, recv, a[0], a[1], a[2], a[3], a[4], a[5])
        }
        7 => {
            let f: extern "C" fn(JNIEnv, JObject, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(env, recv, a[0], a[1], a[2], a[3], a[4], a[5], a[6])
        }
        _ => {
            tracing::error!(
                "JNI call with {} args exceeds maximum supported (8) — returning 0 to prevent undefined behavior",
                a.len()
            );
            0
        }
    }
}

// ---- Index 215: RegisterNatives ----

extern "C" fn jni_register_natives(
    _env: JNIEnv,
    clazz: JClass,
    methods: *const JNINativeMethod,
    n_methods: JInt,
) -> JInt {
    if methods.is_null() || n_methods <= 0 || clazz == 0 {
        return JNI_OK;
    }

    // Resolve the class name from the JClass mirror
    let class_name = with_shared_vm(|shared| {
        let oref = jobject_to_obj(clazz)?;
        let class_id = shared.heap.class_id_of(oref);
        shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.clone())
    })
    .flatten();

    let class_name = match class_name {
        Some(n) => n,
        None => return JNI_ERR,
    };

    for i in 0..n_methods as usize {
        // Safety: caller guarantees `methods` points to `n_methods` valid elements
        let m = unsafe { &*methods.add(i) };
        if m.name.is_null() || m.signature.is_null() || m.fn_ptr.is_null() {
            continue;
        }
        // Safety: name and signature are null-terminated C strings from the JNI caller
        let name = unsafe { CStr::from_ptr(m.name).to_string_lossy().into_owned() };
        let sig = unsafe { CStr::from_ptr(m.signature).to_string_lossy().into_owned() };
        let fn_ptr = m.fn_ptr as usize;
        register_jni_native(&class_name, &name, &sig, fn_ptr);
    }
    JNI_OK
}

// ---- Index 216: UnregisterNatives ----

extern "C" fn jni_unregister_natives(_env: JNIEnv, clazz: JClass) -> JInt {
    if clazz == 0 {
        return JNI_OK;
    }
    // Resolve the class name and remove all registered natives for it.
    // Since JNI_NATIVE_METHODS is keyed by hash, we need the class name
    // to reconstruct the keys. If we can't resolve the class, best-effort no-op.
    let class_name = with_shared_vm(|shared| {
        let oref = jobject_to_obj(clazz)?;
        let class_id = shared.heap.class_id_of(oref);
        shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.clone())
    })
    .flatten();

    if let Some(class_name) = class_name {
        // Get all methods for this class and remove their native registrations
        let methods_to_remove: Vec<u64> = with_shared_vm(|shared| {
            let cm = shared.class_manager.read();
            let mut keys = Vec::new();
            // Find the class and iterate its methods
            if let Some(class_id) = cm.find_class_by_name(&class_name) {
                if let Some(class) = cm.get_class(class_id) {
                    for method in &class.methods {
                        if method.is_native() {
                            let key = jni_native_key(&class_name, &method.name, &method.descriptor);
                            keys.push(key);
                        }
                    }
                }
            }
            keys
        })
        .unwrap_or_default();

        if !methods_to_remove.is_empty() {
            let mut table = JNI_NATIVE_METHODS.write();
            for key in &methods_to_remove {
                table.remove(key);
            }
            tracing::debug!(
                "UnregisterNatives: removed {} native methods for {}",
                methods_to_remove.len(),
                class_name
            );
        }
    }
    JNI_OK
}

// ---------------------------------------------------------------------------
// Missing JNI functions (Phase 2 additions)
// ---------------------------------------------------------------------------

// ---- Index 28: AllocObject ----
// Allocates a new Java object without calling any constructor.
extern "C" fn jni_alloc_object(_env: JNIEnv, clazz: JClass) -> JObject {
    if clazz == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let class_id = ClassId::new(clazz as u32);
        // `clazz` must resolve to a real class. If it does not (stale or
        // bogus handle), return null rather than allocating an object against
        // an unresolved/`ClassId(0)` class: a wrongly-classed object whose
        // declared field count is unknown corrupts every later field access.
        let num_fields = match shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
        {
            Some(n) => n,
            None => {
                tracing::warn!(
                    target: "cratonvm::jni",
                    clazz,
                    "AllocObject: JClass does not resolve to a loaded class"
                );
                return 0;
            }
        };
        let obj = shared.heap.alloc_object(class_id, num_fields);
        obj_to_jobject(obj)
    })
    .unwrap_or(0)
}

// ---- Index 30: NewObjectA ----
// Allocates a new Java object and invokes the constructor indicated by `mid`.
extern "C" fn jni_new_object_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    if clazz == 0 || mid == 0 {
        return 0;
    }
    // First allocate the object.
    let obj_handle = jni_alloc_object(_env, clazz);
    if obj_handle == 0 {
        return 0;
    }
    // Then call the constructor (<init>) on the allocated object.
    with_jni_context(|shared, thread| {
        let oref = jobject_to_obj(obj_handle)?;
        let (decl_class_id, method_index) = decode_method_id(mid);
        let (method_name, descriptor) = {
            let cm = shared.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        let param_types = parse_param_types_cached(&descriptor);
        let mut jvm_args = Vec::with_capacity(1 + param_types.len());
        jvm_args.push(Value::Object(Some(oref)));
        jvm_args.extend(unsafe { jvalues_to_values(args, &param_types) });
        let _ = invoke_on_class_shared(
            shared,
            thread,
            ClassId::new(clazz as u32),
            &method_name,
            &descriptor,
            &jvm_args,
        );
        Some(obj_handle)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Indices 155-158: SetStaticBoolean/Byte/Char/ShortField ----
extern "C" fn jni_set_static_boolean_field(
    _env: JNIEnv,
    _clazz: JClass,
    fid: JFieldID,
    val: JBoolean,
) {
    set_static_int_raw(fid, val as JInt);
}

extern "C" fn jni_set_static_byte_field(
    _env: JNIEnv,
    _clazz: JClass,
    fid: JFieldID,
    val: JByte,
) {
    set_static_int_raw(fid, val as JInt);
}

extern "C" fn jni_set_static_char_field(
    _env: JNIEnv,
    _clazz: JClass,
    fid: JFieldID,
    val: JChar,
) {
    set_static_int_raw(fid, val as JInt);
}

extern "C" fn jni_set_static_short_field(
    _env: JNIEnv,
    _clazz: JClass,
    fid: JFieldID,
    val: JShort,
) {
    set_static_int_raw(fid, val as JInt);
}

// ---- Index 163: NewString (UTF-16) ----
// Creates a java.lang.String from a UTF-16 char array.
extern "C" fn jni_new_string(
    _env: JNIEnv,
    unicode: *const JChar,
    len: JSize,
) -> JString {
    if unicode.is_null() || len < 0 {
        return 0;
    }
    let chars: &[u16] = unsafe { std::slice::from_raw_parts(unicode, len as usize) };
    let s = String::from_utf16_lossy(chars);
    with_shared_vm(|shared| {
        let obj = create_java_string(shared, &s);
        obj_to_jobject(obj)
    })
    .unwrap_or(0)
}

// ---- Index 164: GetStringLength ----
extern "C" fn jni_get_string_length(_env: JNIEnv, str_obj: JString) -> JSize {
    if str_obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.heap, oref)?;
        // Java String length is the UTF-16 code unit count.
        Some(s.chars().map(|c| c.len_utf16()).sum::<usize>() as JSize)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 165/166: GetStringChars / ReleaseStringChars (UTF-16) ----
extern "C" fn jni_get_string_chars(
    _env: JNIEnv,
    str_obj: JString,
    is_copy: *mut JBoolean,
) -> *const JChar {
    if str_obj == 0 {
        return std::ptr::null();
    }
    let result = with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        let s = read_java_string(&shared.heap, oref)?;
        // Encode as UTF-16 and heap-allocate the buffer.
        let utf16: Vec<u16> = s.encode_utf16().collect();
        let len = utf16.len();
        let boxed = utf16.into_boxed_slice();
        let ptr = boxed.as_ptr();
        std::mem::forget(boxed); // OWNERSHIP: buffer transferred to native caller, freed by jni_release_string_chars via Vec::from_raw_parts
        // Track the allocation so ReleaseStringChars can reconstruct the
        // correct Vec layout (pointer + length) for deallocation.
        JNI_STRING_BUFFERS.with(|b| b.borrow_mut().insert(ptr as usize, len));
        Some(ptr)
    })
    .flatten();
    match result {
        Some(ptr) => {
            if !is_copy.is_null() {
                unsafe { *is_copy = JNI_TRUE; }
            }
            ptr
        }
        None => std::ptr::null(),
    }
}

extern "C" fn jni_release_string_chars(
    _env: JNIEnv,
    _str: JString,
    chars: *const JChar,
) {
    if !chars.is_null() {
        // Look up the original element count so we can reconstruct the Vec
        // with the correct layout.  Without the length the global allocator
        // would receive a mismatched dealloc (single-element vs slice).
        let len = JNI_STRING_BUFFERS.with(|b| {
            b.borrow_mut().remove(&(chars as usize))
        }).unwrap_or(0);
        if len > 0 {
            unsafe {
                drop(Vec::from_raw_parts(chars as *mut JChar, len, len));
            }
        }
        // len == 0 means the pointer wasn't tracked (shouldn't happen in
        // correct usage).  We intentionally leak rather than corrupt the
        // allocator with a wrong layout.
    }
}

// ---------------------------------------------------------------------------
// va_list helpers — extract JNI arguments from a C va_list pointer
// ---------------------------------------------------------------------------
//
// On x86-64 Windows the calling convention passes a va_list as a simple
// pointer to an 8-byte-aligned argument block.  Each slot is 8 bytes wide
// regardless of the actual argument type.
//
// On x86-64 SysV (Linux/macOS) va_list is a *struct { gp_offset, fp_offset,
// overflow_arg_area, reg_save_area }*, which is considerably more complex.
// For portability we define both paths and use cfg to select.

/// Opaque C va_list pointer.
pub type VaList = *mut u8;

/// Read a JNI method's arguments from a va_list and pack them into a JValue
/// array.  The caller is responsible for passing the correct `mid` so that we
/// can look up the method descriptor and determine argument types.
///
/// Returns `std::ptr::null()` on failure; otherwise returns a heap-allocated
/// JValue slice that the caller must free with `Box::from_raw`.
fn va_list_to_jvalues(mid: JMethodID, mut va: VaList) -> (*const JValue, usize) {
    if mid == 0 || va.is_null() {
        return (std::ptr::null(), 0);
    }
    // Look up the method descriptor to know argument types.
    let descriptor = with_shared_vm(|shared| {
        let (decl_class_id, method_index) = decode_method_id(mid);
        let cm = shared.class_manager.read();
        let class = cm.class_store.get(decl_class_id)?;
        let method = class.methods.get(method_index as usize)?;
        Some(method.descriptor.clone())
    })
    .flatten();
    let descriptor = match descriptor {
        Some(d) => d,
        None => return (std::ptr::null(), 0),
    };
    let param_types = parse_param_types_cached(&descriptor);
    if param_types.is_empty() {
        return (std::ptr::null(), 0);
    }
    let mut jvalues: Vec<JValue> = Vec::with_capacity(param_types.len());
    for &tag in &param_types {
        // On all supported platforms, va_list args are 8-byte slots.
        let raw: u64 = unsafe {
            let val = *(va as *const u64);
            va = va.add(8);
            val
        };
        let jv = match tag {
            b'Z' => JValue { z: raw as JBoolean },
            b'B' => JValue { b: raw as JByte },
            b'C' => JValue { c: raw as JChar },
            b'S' => JValue { s: raw as JShort },
            b'I' => JValue { i: raw as JInt },
            b'J' => JValue { j: raw as JLong },
            b'F' => JValue { f: f32::from_bits(raw as u32) },
            b'D' => JValue { d: f64::from_bits(raw) },
            _ /* L, [ */ => JValue { l: raw },
        };
        jvalues.push(jv);
    }
    let len = jvalues.len();
    let boxed = jvalues.into_boxed_slice();
    let ptr = boxed.as_ptr();
    std::mem::forget(boxed); // OWNERSHIP: buffer transferred to caller, freed by free_jvalues via Box::from_raw
    (ptr, len)
}

/// Free a JValue array returned by `va_list_to_jvalues`.
unsafe fn free_jvalues(ptr: *const JValue, len: usize) {
    if !ptr.is_null() && len > 0 {
        drop(Box::from_raw(std::slice::from_raw_parts_mut(
            ptr as *mut JValue,
            len,
        )));
    }
}

// ---------------------------------------------------------------------------
// V-variant method call wrappers (take va_list instead of JValue*)
// ---------------------------------------------------------------------------

// --- NewObjectV (slot 29) ---
extern "C" fn jni_new_object_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let result = jni_new_object_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    result
}

// --- Instance CallXxxMethodV (slots 35, 38, 41, 44, 47, 50, 53, 56, 59, 62) ---
extern "C" fn jni_call_object_method_v(
    env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList,
) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_object_method_a(env, obj, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_boolean_method_v(
    env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList,
) -> JBoolean {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_boolean_method_a(env, obj, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_byte_method_v(
    env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList,
) -> JByte {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_byte_method_a(env, obj, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_char_method_v(
    env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList,
) -> JChar {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_char_method_a(env, obj, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_short_method_v(
    env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList,
) -> JShort {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_short_method_a(env, obj, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_int_method_v(
    env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList,
) -> JInt {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_int_method_a(env, obj, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_long_method_v(
    env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList,
) -> JLong {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_long_method_a(env, obj, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_float_method_v(
    env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList,
) -> JFloat {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_float_method_a(env, obj, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_double_method_v(
    env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList,
) -> JDouble {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_double_method_a(env, obj, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_void_method_v(
    env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList,
) {
    let (args, len) = va_list_to_jvalues(mid, va);
    jni_call_void_method_a(env, obj, mid, args);
    unsafe { free_jvalues(args, len); }
}

// --- Nonvirtual CallNonvirtualXxxMethodV (slots 65, 68, 71, 74, 77, 80, 83, 86, 89, 92) ---
extern "C" fn jni_call_nonvirtual_object_method_v(
    env: JNIEnv, obj: JObject, clazz: JClass, mid: JMethodID, va: VaList,
) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_object_method_a(env, obj, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_nonvirtual_boolean_method_v(
    env: JNIEnv, obj: JObject, clazz: JClass, mid: JMethodID, va: VaList,
) -> JBoolean {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_boolean_method_a(env, obj, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_nonvirtual_byte_method_v(
    env: JNIEnv, obj: JObject, clazz: JClass, mid: JMethodID, va: VaList,
) -> JByte {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_byte_method_a(env, obj, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_nonvirtual_char_method_v(
    env: JNIEnv, obj: JObject, clazz: JClass, mid: JMethodID, va: VaList,
) -> JChar {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_char_method_a(env, obj, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_nonvirtual_short_method_v(
    env: JNIEnv, obj: JObject, clazz: JClass, mid: JMethodID, va: VaList,
) -> JShort {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_short_method_a(env, obj, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_nonvirtual_int_method_v(
    env: JNIEnv, obj: JObject, clazz: JClass, mid: JMethodID, va: VaList,
) -> JInt {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_int_method_a(env, obj, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_nonvirtual_long_method_v(
    env: JNIEnv, obj: JObject, clazz: JClass, mid: JMethodID, va: VaList,
) -> JLong {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_long_method_a(env, obj, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_nonvirtual_float_method_v(
    env: JNIEnv, obj: JObject, clazz: JClass, mid: JMethodID, va: VaList,
) -> JFloat {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_float_method_a(env, obj, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_nonvirtual_double_method_v(
    env: JNIEnv, obj: JObject, clazz: JClass, mid: JMethodID, va: VaList,
) -> JDouble {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_double_method_a(env, obj, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_nonvirtual_void_method_v(
    env: JNIEnv, obj: JObject, clazz: JClass, mid: JMethodID, va: VaList,
) {
    let (args, len) = va_list_to_jvalues(mid, va);
    jni_call_nonvirtual_void_method_a(env, obj, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
}

// --- Static CallStaticXxxMethodV (slots 115, 118, 121, 124, 127, 130, 133, 136, 139, 142) ---
extern "C" fn jni_call_static_object_method_v(
    env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList,
) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_object_method_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_static_boolean_method_v(
    env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList,
) -> JBoolean {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_boolean_method_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_static_byte_method_v(
    env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList,
) -> JByte {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_byte_method_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_static_char_method_v(
    env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList,
) -> JChar {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_char_method_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_static_short_method_v(
    env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList,
) -> JShort {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_short_method_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_static_int_method_v(
    env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList,
) -> JInt {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_int_method_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_static_long_method_v(
    env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList,
) -> JLong {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_long_method_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_static_float_method_v(
    env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList,
) -> JFloat {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_float_method_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_static_double_method_v(
    env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList,
) -> JDouble {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_double_method_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
    r
}

extern "C" fn jni_call_static_void_method_v(
    env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList,
) {
    let (args, len) = va_list_to_jvalues(mid, va);
    jni_call_static_void_method_a(env, clazz, mid, args);
    unsafe { free_jvalues(args, len); }
}

// ---------------------------------------------------------------------------
// NIO Direct ByteBuffer support (slots 229-231)
// ---------------------------------------------------------------------------
//
// DirectByteBuffer objects are represented as regular Java objects with two
// special fields: a native memory address (long) and a capacity (int).
// We store these in fields [0] (address as long) and [1] (capacity as long).

/// Wrapper around a raw pointer to make it Send+Sync for the global registry.
/// Safety: direct buffer memory is allocated via malloc and is valid for the lifetime of the buffer.
struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

/// Global registry of direct buffer metadata: maps JObject handle → (address, capacity).
static DIRECT_BUFFERS: std::sync::LazyLock<
    parking_lot::Mutex<HashMap<u64, (SendPtr, i64)>>
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

// Index 229: NewDirectByteBuffer
extern "C" fn jni_new_direct_byte_buffer(
    _env: JNIEnv,
    address: *mut u8,
    capacity: JLong,
) -> JObject {
    if address.is_null() || capacity < 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        // Allocate a java/nio/DirectByteBuffer-like object with 2 fields
        // (address, capacity). The object's class must declare those 2
        // fields — allocating with `ClassId::new(0)` (`java/lang/Object`,
        // zero declared fields) yields an undersized object that the GC's
        // `get_field` bounds guard rejects on every access.
        let dbb_class_id = shared
            .load_class_concurrent("java/nio/DirectByteBuffer")
            .unwrap_or_else(|_| {
                shared
                    .class_manager
                    .write()
                    .ensure_synthetic_class("java/nio/DirectByteBuffer", 2)
            });
        let num_fields = shared
            .class_manager
            .read()
            .get_class(dbb_class_id)
            .map_or(2, |c| c.num_total_fields.max(2));
        let obj = shared.heap.alloc_object(dbb_class_id, num_fields);
        let handle = obj_to_jobject(obj);
        // Store the address as a long in field 0.
        shared.heap.set_field(obj, 0, Value::Long(address as i64));
        // Store the capacity in field 1.
        shared.heap.set_field(obj, 1, Value::Long(capacity));
        // Also register in our side-table for GetDirectBufferAddress.
        DIRECT_BUFFERS.lock().insert(handle, (SendPtr(address), capacity));
        handle
    })
    .unwrap_or(0)
}

// Index 230: GetDirectBufferAddress
extern "C" fn jni_get_direct_buffer_address(
    _env: JNIEnv,
    buf: JObject,
) -> *mut u8 {
    if buf == 0 {
        return std::ptr::null_mut();
    }
    // First try the side-table (fast path for buffers we created).
    if let Some(&(SendPtr(addr), _)) = DIRECT_BUFFERS.lock().get(&buf) {
        return addr;
    }
    // Fall back to reading the address field from the object.
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(buf)?;
        match shared.heap.get_field(oref, 0) {
            Value::Long(addr) => Some(addr as *mut u8),
            _ => None,
        }
    })
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

// Index 231: GetDirectBufferCapacity
extern "C" fn jni_get_direct_buffer_capacity(
    _env: JNIEnv,
    buf: JObject,
) -> JLong {
    if buf == 0 {
        return -1;
    }
    // First try the side-table.
    if let Some(&(_, cap)) = DIRECT_BUFFERS.lock().get(&buf) {
        return cap;
    }
    // Fall back to reading the capacity field.
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(buf)?;
        match shared.heap.get_field(oref, 1) {
            Value::Long(cap) => Some(cap),
            _ => None,
        }
    })
    .flatten()
    .unwrap_or(-1)
}

// Index 232: GetObjectRefType (correct JNI 1.6 slot)
// Already implemented at index 19 (jni_get_object_ref_type) — we alias it here.

// Index 233: GetModule (JNI 9+)
// Returns the java.lang.Module that the class belongs to.
// For now, we return null (unnamed module) since our module system is basic.
extern "C" fn jni_get_module(
    _env: JNIEnv,
    _clazz: JClass,
) -> JObject {
    // All classes are in the unnamed module for now.
    0
}

// ---------------------------------------------------------------------------
// JNI Function Table (flat array)
// ---------------------------------------------------------------------------

/// Build the JNI function table at runtime.
fn build_function_table() -> Box<[usize; JNI_FUNCTION_COUNT]> {
    let stub = jni_stub as *const () as usize;
    let mut t = [stub; JNI_FUNCTION_COUNT];

    // Version
    t[4] = jni_get_version as *const () as usize;

    // Class operations
    t[5] = jni_define_class as *const () as usize;
    t[6] = jni_find_class as *const () as usize;
    t[7] = jni_from_reflected_method as *const () as usize;
    t[8] = jni_from_reflected_field as *const () as usize;
    t[9] = jni_to_reflected_method as *const () as usize;
    t[10] = jni_get_superclass as *const () as usize;
    t[11] = jni_is_assignable_from as *const () as usize;
    t[12] = jni_to_reflected_field as *const () as usize;

    // Exceptions
    t[13] = jni_throw as *const () as usize;
    t[14] = jni_throw_new as *const () as usize;
    t[15] = jni_exception_occurred as *const () as usize;
    t[16] = jni_exception_describe as *const () as usize;
    t[17] = jni_exception_clear as *const () as usize;
    t[18] = jni_fatal_error as *const () as usize;

    // GetObjectRefType
    t[19] = jni_get_object_ref_type as *const () as usize;

    // Local/global frame management
    t[20] = jni_push_local_frame as *const () as usize;
    t[21] = jni_pop_local_frame as *const () as usize;

    // References
    t[22] = jni_new_global_ref as *const () as usize;
    t[23] = jni_delete_global_ref as *const () as usize;
    t[24] = jni_delete_local_ref as *const () as usize;
    t[25] = jni_is_same_object as *const () as usize;
    t[26] = jni_new_local_ref as *const () as usize;
    t[27] = jni_ensure_local_capacity as *const () as usize;
    t[28] = jni_alloc_object as *const () as usize;
    // t[28] = NewObject (varargs) — not implementable in stable Rust extern "C"
    t[29] = jni_new_object_v as *const () as usize;
    t[30] = jni_new_object_a as *const () as usize;

    // Object operations
    t[31] = jni_get_object_class as *const () as usize;
    t[32] = jni_is_instance_of as *const () as usize;

    // Method IDs
    t[33] = jni_get_method_id as *const () as usize;

    // Call<Type>Method/V/A — virtual instance (groups of 3: varargs, va_list, array)
    // varargs slots (34,37,40,...) left as stubs — not implementable in stable Rust
    t[35] = jni_call_object_method_v as *const () as usize;
    t[36] = jni_call_object_method_a as *const () as usize;
    t[38] = jni_call_boolean_method_v as *const () as usize;
    t[39] = jni_call_boolean_method_a as *const () as usize;
    t[41] = jni_call_byte_method_v as *const () as usize;
    t[42] = jni_call_byte_method_a as *const () as usize;
    t[44] = jni_call_char_method_v as *const () as usize;
    t[45] = jni_call_char_method_a as *const () as usize;
    t[47] = jni_call_short_method_v as *const () as usize;
    t[48] = jni_call_short_method_a as *const () as usize;
    t[50] = jni_call_int_method_v as *const () as usize;
    t[51] = jni_call_int_method_a as *const () as usize;
    t[53] = jni_call_long_method_v as *const () as usize;
    t[54] = jni_call_long_method_a as *const () as usize;
    t[56] = jni_call_float_method_v as *const () as usize;
    t[57] = jni_call_float_method_a as *const () as usize;
    t[59] = jni_call_double_method_v as *const () as usize;
    t[60] = jni_call_double_method_a as *const () as usize;
    t[62] = jni_call_void_method_v as *const () as usize;
    t[63] = jni_call_void_method_a as *const () as usize;

    // CallNonvirtual<Type>Method/V/A (groups of 3)
    t[65] = jni_call_nonvirtual_object_method_v as *const () as usize;
    t[66] = jni_call_nonvirtual_object_method_a as *const () as usize;
    t[68] = jni_call_nonvirtual_boolean_method_v as *const () as usize;
    t[69] = jni_call_nonvirtual_boolean_method_a as *const () as usize;
    t[71] = jni_call_nonvirtual_byte_method_v as *const () as usize;
    t[72] = jni_call_nonvirtual_byte_method_a as *const () as usize;
    t[74] = jni_call_nonvirtual_char_method_v as *const () as usize;
    t[75] = jni_call_nonvirtual_char_method_a as *const () as usize;
    t[77] = jni_call_nonvirtual_short_method_v as *const () as usize;
    t[78] = jni_call_nonvirtual_short_method_a as *const () as usize;
    t[80] = jni_call_nonvirtual_int_method_v as *const () as usize;
    t[81] = jni_call_nonvirtual_int_method_a as *const () as usize;
    t[83] = jni_call_nonvirtual_long_method_v as *const () as usize;
    t[84] = jni_call_nonvirtual_long_method_a as *const () as usize;
    t[86] = jni_call_nonvirtual_float_method_v as *const () as usize;
    t[87] = jni_call_nonvirtual_float_method_a as *const () as usize;
    t[89] = jni_call_nonvirtual_double_method_v as *const () as usize;
    t[90] = jni_call_nonvirtual_double_method_a as *const () as usize;
    t[92] = jni_call_nonvirtual_void_method_v as *const () as usize;
    t[93] = jni_call_nonvirtual_void_method_a as *const () as usize;

    // CallStatic<Type>Method/V/A (groups of 3)
    t[115] = jni_call_static_object_method_v as *const () as usize;
    t[116] = jni_call_static_object_method_a as *const () as usize;
    t[118] = jni_call_static_boolean_method_v as *const () as usize;
    t[119] = jni_call_static_boolean_method_a as *const () as usize;
    t[121] = jni_call_static_byte_method_v as *const () as usize;
    t[122] = jni_call_static_byte_method_a as *const () as usize;
    t[124] = jni_call_static_char_method_v as *const () as usize;
    t[125] = jni_call_static_char_method_a as *const () as usize;
    t[127] = jni_call_static_short_method_v as *const () as usize;
    t[128] = jni_call_static_short_method_a as *const () as usize;
    t[130] = jni_call_static_int_method_v as *const () as usize;
    t[131] = jni_call_static_int_method_a as *const () as usize;
    t[133] = jni_call_static_long_method_v as *const () as usize;
    t[134] = jni_call_static_long_method_a as *const () as usize;
    t[136] = jni_call_static_float_method_v as *const () as usize;
    t[137] = jni_call_static_float_method_a as *const () as usize;
    t[139] = jni_call_static_double_method_v as *const () as usize;
    t[140] = jni_call_static_double_method_a as *const () as usize;
    t[142] = jni_call_static_void_method_v as *const () as usize;
    t[143] = jni_call_static_void_method_a as *const () as usize;

    // Instance field access
    t[94] = jni_get_field_id as *const () as usize;
    t[95] = jni_get_object_field as *const () as usize;
    t[96] = jni_get_boolean_field as *const () as usize;
    t[97] = jni_get_byte_field as *const () as usize;
    t[98] = jni_get_char_field as *const () as usize;
    t[99] = jni_get_short_field as *const () as usize;
    t[100] = jni_get_int_field as *const () as usize;
    t[101] = jni_get_long_field as *const () as usize;
    t[102] = jni_get_float_field as *const () as usize;
    t[103] = jni_get_double_field as *const () as usize;
    t[104] = jni_set_object_field as *const () as usize;
    t[105] = jni_set_boolean_field as *const () as usize;
    t[106] = jni_set_byte_field as *const () as usize;
    t[107] = jni_set_char_field as *const () as usize;
    t[108] = jni_set_short_field as *const () as usize;
    t[109] = jni_set_int_field as *const () as usize;
    t[110] = jni_set_long_field as *const () as usize;
    t[111] = jni_set_float_field as *const () as usize;
    t[112] = jni_set_double_field as *const () as usize;

    // Static method IDs
    t[113] = jni_get_static_method_id as *const () as usize;

    // Static field access
    t[144] = jni_get_static_field_id as *const () as usize;
    t[145] = jni_get_static_object_field as *const () as usize;
    t[146] = jni_get_static_boolean_field as *const () as usize;
    t[147] = jni_get_static_byte_field as *const () as usize;
    t[148] = jni_get_static_char_field as *const () as usize;
    t[149] = jni_get_static_short_field as *const () as usize;
    t[150] = jni_get_static_int_field as *const () as usize;
    t[151] = jni_get_static_long_field as *const () as usize;
    t[152] = jni_get_static_float_field as *const () as usize;
    t[153] = jni_get_static_double_field as *const () as usize;
    t[154] = jni_set_static_object_field as *const () as usize;
    t[155] = jni_set_static_boolean_field as *const () as usize;
    t[156] = jni_set_static_byte_field as *const () as usize;
    t[157] = jni_set_static_char_field as *const () as usize;
    t[158] = jni_set_static_short_field as *const () as usize;
    t[159] = jni_set_static_int_field as *const () as usize;
    t[160] = jni_set_static_long_field as *const () as usize;
    t[161] = jni_set_static_float_field as *const () as usize;
    t[162] = jni_set_static_double_field as *const () as usize;

    // String operations
    t[163] = jni_new_string as *const () as usize;
    t[164] = jni_get_string_length as *const () as usize;
    t[165] = jni_get_string_chars as *const () as usize;
    t[166] = jni_release_string_chars as *const () as usize;
    t[167] = jni_new_string_utf as *const () as usize;
    t[168] = jni_get_string_utf_length as *const () as usize;
    t[169] = jni_get_string_utf_chars as *const () as usize;
    t[170] = jni_release_string_utf_chars as *const () as usize;

    // Array operations
    t[171] = jni_get_array_length as *const () as usize;
    t[172] = jni_new_object_array as *const () as usize;
    t[173] = jni_get_object_array_element as *const () as usize;
    t[174] = jni_set_object_array_element as *const () as usize;
    t[175] = jni_new_boolean_array as *const () as usize;
    t[176] = jni_new_byte_array as *const () as usize;
    t[177] = jni_new_char_array as *const () as usize;
    t[178] = jni_new_short_array as *const () as usize;
    t[179] = jni_new_int_array as *const () as usize;
    t[180] = jni_new_long_array as *const () as usize;
    t[181] = jni_new_float_array as *const () as usize;
    t[182] = jni_new_double_array as *const () as usize;
    t[183] = jni_get_boolean_array_elements as *const () as usize;
    t[184] = jni_get_byte_array_elements as *const () as usize;
    t[185] = jni_get_char_array_elements as *const () as usize;
    t[186] = jni_get_short_array_elements as *const () as usize;
    t[187] = jni_get_int_array_elements as *const () as usize;
    t[188] = jni_get_long_array_elements as *const () as usize;
    t[189] = jni_get_float_array_elements as *const () as usize;
    t[190] = jni_get_double_array_elements as *const () as usize;
    t[191] = jni_release_boolean_array_elements as *const () as usize;
    t[192] = jni_release_byte_array_elements as *const () as usize;
    t[193] = jni_release_char_array_elements as *const () as usize;
    t[194] = jni_release_short_array_elements as *const () as usize;
    t[195] = jni_release_int_array_elements as *const () as usize;
    t[196] = jni_release_long_array_elements as *const () as usize;
    t[197] = jni_release_float_array_elements as *const () as usize;
    t[198] = jni_release_double_array_elements as *const () as usize;
    t[199] = jni_get_boolean_array_region as *const () as usize;
    t[200] = jni_get_byte_array_region as *const () as usize;
    t[201] = jni_get_char_array_region as *const () as usize;
    t[202] = jni_get_short_array_region as *const () as usize;
    t[203] = jni_get_int_array_region as *const () as usize;
    t[204] = jni_get_long_array_region as *const () as usize;
    t[205] = jni_get_float_array_region as *const () as usize;
    t[206] = jni_get_double_array_region as *const () as usize;
    t[207] = jni_set_boolean_array_region as *const () as usize;
    t[208] = jni_set_byte_array_region as *const () as usize;
    t[209] = jni_set_char_array_region as *const () as usize;
    t[210] = jni_set_short_array_region as *const () as usize;
    t[211] = jni_set_int_array_region as *const () as usize;
    t[212] = jni_set_long_array_region as *const () as usize;
    t[213] = jni_set_float_array_region as *const () as usize;
    t[214] = jni_set_double_array_region as *const () as usize;

    // RegisterNatives / UnregisterNatives
    t[215] = jni_register_natives as *const () as usize;
    t[216] = jni_unregister_natives as *const () as usize;

    // Synchronization
    t[217] = jni_monitor_enter as *const () as usize;
    t[218] = jni_monitor_exit as *const () as usize;

    // JavaVM
    t[219] = jni_get_java_vm as *const () as usize;

    // String region operations
    t[220] = jni_get_string_region as *const () as usize;
    t[221] = jni_get_string_utf_region as *const () as usize;

    // Critical array / string operations
    t[222] = jni_get_primitive_array_critical as *const () as usize;
    t[223] = jni_release_primitive_array_critical as *const () as usize;
    t[224] = jni_get_string_critical as *const () as usize;
    t[225] = jni_release_string_critical as *const () as usize;

    // Weak global references
    t[226] = jni_new_weak_global_ref as *const () as usize;
    t[227] = jni_delete_weak_global_ref as *const () as usize;

    // Exception check
    t[228] = jni_exception_check as *const () as usize;

    // NIO Direct ByteBuffer (JNI 1.4)
    t[229] = jni_new_direct_byte_buffer as *const () as usize;
    t[230] = jni_get_direct_buffer_address as *const () as usize;
    t[231] = jni_get_direct_buffer_capacity as *const () as usize;

    // GetObjectRefType (JNI 1.6) — also at slot 19 for compatibility
    t[232] = jni_get_object_ref_type as *const () as usize;

    // GetModule (JNI 9+)
    t[233] = jni_get_module as *const () as usize;

    Box::new(t)
}

// ---------------------------------------------------------------------------
// JavaVM Invocation Interface
// ---------------------------------------------------------------------------

extern "C" fn jni_destroy_java_vm(_vm: JavaVM) -> JInt {
    JNI_OK
}

extern "C" fn jni_get_env(_vm: JavaVM, env: *mut *mut std::ffi::c_void, _version: JInt) -> JInt {
    if env.is_null() {
        return JNI_ERR;
    }
    let jni_env = get_jni_env();
    unsafe {
        *env = jni_env as *mut std::ffi::c_void;
    }
    JNI_OK
}

extern "C" fn jni_attach_current_thread(
    _vm: JavaVM,
    penv: *mut *mut std::ffi::c_void,
    _args: *mut std::ffi::c_void,
) -> JInt {
    // Set up JNI environment pointer for the current thread.
    // If context is already set, this is a no-op (thread already attached).
    let has_context = JNI_SHARED_VM.with(|c| c.borrow().is_some());
    if has_context {
        // Already attached — just return the existing env pointer
        if !penv.is_null() {
            init_jni_table();
            let table = JNI_TABLE_PTR.load(std::sync::atomic::Ordering::Acquire);
            unsafe {
                *penv = table as *mut std::ffi::c_void;
            }
        }
        tracing::trace!("JNI AttachCurrentThread: thread already attached");
        return JNI_OK;
    }
    // Thread not yet attached — set up env pointer
    // Note: full thread registration with the VM's ThreadRegistry requires
    // access to SharedVm which is obtained from the JavaVM* pointer.
    if !penv.is_null() {
        init_jni_table();
        let table = JNI_TABLE_PTR.load(std::sync::atomic::Ordering::Acquire);
        unsafe {
            *penv = table as *mut std::ffi::c_void;
        }
    }
    tracing::debug!("JNI AttachCurrentThread: thread attached");
    JNI_OK
}

extern "C" fn jni_detach_current_thread(_vm: JavaVM) -> JInt {
    // Clear the JNI thread-local context for this thread.
    let had_context = JNI_SHARED_VM.with(|c| c.borrow().is_some());
    if had_context {
        clear_jni_context();
        clear_jni_thread();
        tracing::debug!("JNI DetachCurrentThread: thread detached");
    } else {
        tracing::trace!("JNI DetachCurrentThread: thread was not attached");
    }
    JNI_OK
}

fn build_invoke_table() -> Box<[usize; JNI_INVOKE_FUNCTION_COUNT]> {
    let stub = jni_stub as *const () as usize;
    let mut t = [stub; JNI_INVOKE_FUNCTION_COUNT];
    t[3] = jni_destroy_java_vm as *const () as usize;
    t[4] = jni_attach_current_thread as *const () as usize;
    t[5] = jni_detach_current_thread as *const () as usize;
    t[6] = jni_get_env as *const () as usize;
    t[7] = jni_attach_current_thread as *const () as usize; // AttachCurrentThreadAsDaemon
    Box::new(t)
}

// ---------------------------------------------------------------------------
// Global table storage
// ---------------------------------------------------------------------------

// The two JNI function tables are leaked, process-lifetime singletons. They are
// stored in `AtomicPtr`s rather than `static mut` so that reads/writes are
// well-defined under concurrent access (raw `static mut` access is UB-adjacent
// on the 2024 edition). `JNI_TABLE_INIT` (a `Once`) still guarantees the values
// are written exactly once; the atomics only make the publication well-defined.
static JNI_TABLE_PTR: std::sync::atomic::AtomicPtr<usize> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static JNI_INVOKE_TABLE_PTR: std::sync::atomic::AtomicPtr<usize> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static JNI_TABLE_INIT: std::sync::Once = std::sync::Once::new();

fn init_jni_table() {
    use std::sync::atomic::Ordering;
    JNI_TABLE_INIT.call_once(|| {
        let table = build_function_table();
        let raw = Box::into_raw(table) as *mut usize; // OWNERSHIP: transferred to JNI_TABLE_PTR static; intentionally leaked (process-lifetime singleton)
        let invoke_table = build_invoke_table();
        let invoke_raw = Box::into_raw(invoke_table) as *mut usize; // OWNERSHIP: transferred to JNI_INVOKE_TABLE_PTR static; intentionally leaked (process-lifetime singleton)
        JNI_TABLE_PTR.store(raw, Ordering::Release);
        JNI_INVOKE_TABLE_PTR.store(invoke_raw, Ordering::Release);
    });
}

/// Get a JNIEnv pointer.
/// JNIEnv = `*const *const usize` — a pointer to a pointer to the function table.
pub fn get_jni_env() -> JNIEnv {
    init_jni_table();
    // `AtomicPtr<usize>` has the same layout as `*mut usize`, so the address of
    // the atomic itself is a valid `*const *const usize` pointing at the table
    // pointer published by `init_jni_table`.
    std::ptr::addr_of!(JNI_TABLE_PTR).cast()
}

/// Get a JavaVM pointer.
pub fn get_java_vm() -> JavaVM {
    init_jni_table();
    std::ptr::addr_of!(JNI_INVOKE_TABLE_PTR).cast()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn global_refs_add_remove() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        // Handle must be non-zero and have the global-ref tag bit set.
        assert_ne!(handle, 0);
        assert_eq!(handle & 1, 1, "global ref handle must have bit 0 set");
        // Resolving the handle must yield the original object.
        assert_eq!(refs.resolve(handle), Some(obj));
        assert_eq!(refs.count(), 1);
        // Removal succeeds and the handle no longer resolves.
        assert!(refs.remove(handle));
        assert_eq!(refs.count(), 0);
    }

    #[test]
    fn local_frame_lifecycle() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut frame = JniLocalFrame::new();
        let jobj = frame.add(obj);
        assert_ne!(jobj, 0);
        assert_eq!(frame.refs.len(), 1);
        frame.remove(jobj);
        assert_eq!(frame.refs.len(), 0);
    }

    #[test]
    fn jobject_null_roundtrip() {
        assert!(jobject_to_obj(0).is_none());
    }

    #[test]
    fn jni_version_constant() {
        assert_eq!(JNI_VERSION_1_8, 0x00010008);
    }

    #[test]
    fn jni_function_table_get_version() {
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(4) };
        assert_ne!(func_ptr, 0);
        let get_version: extern "C" fn(JNIEnv) -> JInt = unsafe { std::mem::transmute(func_ptr) };
        assert_eq!(get_version(env), JNI_VERSION_1_8);
    }

    #[test]
    fn jni_function_table_exception_check() {
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(228) };
        let exception_check: extern "C" fn(JNIEnv) -> JBoolean =
            unsafe { std::mem::transmute(func_ptr) };
        assert_eq!(exception_check(env), JNI_FALSE);
    }

    #[test]
    fn jni_function_table_is_same_object() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::Arc;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj1 = shared.heap.alloc_object(crate::classloading::ClassId::new(0), 1);
        let obj2 = shared.heap.alloc_object(crate::classloading::ClassId::new(0), 1);
        let local1 = obj_to_jobject(obj1);
        let local2 = obj_to_jobject(obj2);
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(25) };
        let is_same: extern "C" fn(JNIEnv, JObject, JObject) -> JBoolean =
            unsafe { std::mem::transmute(func_ptr) };
        assert_eq!(is_same(env, local1, local1), JNI_TRUE);
        assert_eq!(is_same(env, local1, local2), JNI_FALSE);
        assert_eq!(is_same(env, 0, 0), JNI_TRUE); // null == null
        clear_jni_context();
    }

    #[test]
    fn jni_function_table_stub_returns_zero() {
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(0) };
        let stub: extern "C" fn() -> usize = unsafe { std::mem::transmute(func_ptr) };
        assert_eq!(stub(), 0);
    }

    #[test]
    fn global_refs_multiple_entries() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj1 = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let obj2 = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut refs = JniGlobalRefs::new();
        let h1 = refs.add(obj1);
        let h2 = refs.add(obj2);
        assert_ne!(h1, h2);
        assert_eq!(refs.count(), 2);
        // Each handle resolves to the correct object.
        assert_eq!(refs.resolve(h1), Some(obj1));
        assert_eq!(refs.resolve(h2), Some(obj2));
    }

    #[test]
    fn global_refs_remove_unknown_handle_returns_false() {
        let mut refs = JniGlobalRefs::new();
        // 0xDEAD_BEEF1 has bit 0 set (looks like a global-ref handle) but was never added.
        assert!(!refs.remove(0xDEAD_BEEF1));
        // A local-ref handle (bit 0 = 0) also returns false.
        assert!(!refs.remove(8));
    }

    #[test]
    fn global_refs_default_trait() {
        let refs = JniGlobalRefs::default();
        assert_eq!(refs.count(), 0);
    }

    #[test]
    fn local_frame_default_trait() {
        let frame = JniLocalFrame::default();
        assert_eq!(frame.refs.len(), 0);
    }

    #[test]
    fn method_id_encode_decode() {
        let class_id = ClassId::new(42);
        let method_index = 7u16;
        let mid = encode_method_id(class_id, method_index);
        let (decoded_class, decoded_idx) = decode_method_id(mid);
        assert_eq!(decoded_class, class_id);
        assert_eq!(decoded_idx, method_index);
    }

    #[test]
    fn field_id_encode_decode() {
        let class_id = ClassId::new(100);
        let field_index = 3usize;
        let fid = encode_field_id(class_id, field_index);
        let (decoded_class, decoded_idx) = decode_field_id(fid);
        assert_eq!(decoded_class, class_id);
        assert_eq!(decoded_idx, field_index);
    }

    #[test]
    fn jni_get_array_length_with_context() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let arr = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 10);
        let jarray = obj_to_jobject(arr);
        let env = get_jni_env();
        let len = jni_get_array_length(env, jarray);
        assert_eq!(len, 10);
        clear_jni_context();
    }

    #[test]
    fn jni_field_access_with_context() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let obj = shared.heap.alloc_object(ClassId::new(0), 3);
        let jobj = obj_to_jobject(obj);
        let fid = encode_field_id(ClassId::new(0), 1);
        let env = get_jni_env();
        jni_set_int_field(env, jobj, fid, 42);
        let val = jni_get_int_field(env, jobj, fid);
        assert_eq!(val, 42);
        clear_jni_context();
    }

    #[test]
    fn jni_new_int_array_with_context() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        let arr = jni_new_int_array(env, 5);
        assert_ne!(arr, 0);
        let len = jni_get_array_length(env, arr);
        assert_eq!(len, 5);
        clear_jni_context();
    }

    #[test]
    fn jni_array_region_roundtrip() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        let arr = jni_new_int_array(env, 4);
        let data: [JInt; 4] = [10, 20, 30, 40];
        jni_set_int_array_region(env, arr, 0, 4, data.as_ptr());
        let mut out = [0i32; 4];
        jni_get_int_array_region(env, arr, 0, 4, out.as_mut_ptr());
        assert_eq!(out, [10, 20, 30, 40]);
        clear_jni_context();
    }

    #[test]
    fn jni_java_vm_get_env() {
        let env = get_jni_env();
        let mut vm_ptr: JavaVM = std::ptr::null();
        let result = jni_get_java_vm(env, &mut vm_ptr);
        assert_eq!(result, JNI_OK);
        assert!(!vm_ptr.is_null());
    }

    #[test]
    fn jni_invoke_table_get_env() {
        let vm = get_java_vm();
        let func_ptr = unsafe { *(*vm).add(6) };
        assert_ne!(func_ptr, 0);
    }

    // -----------------------------------------------------------------------
    // M2: RegisterNatives / find_jni_native tests
    // -----------------------------------------------------------------------

    #[test]
    fn register_and_find_jni_native() {
        // Register a fake function pointer and verify lookup works.
        extern "C" fn fake_native(_env: JNIEnv, _this: JObject) -> u64 {
            42
        }
        let fn_ptr = fake_native as *const () as usize;
        register_jni_native("com/example/Foo", "bar", "()J", fn_ptr);
        assert_eq!(
            find_jni_native("com/example/Foo", "bar", "()J"),
            Some(fn_ptr)
        );
        // Different class → not found
        assert!(find_jni_native("com/example/Other", "bar", "()J").is_none());
        // Different descriptor → not found
        assert!(find_jni_native("com/example/Foo", "bar", "()V").is_none());
    }

    #[test]
    fn register_jni_native_overwrites() {
        extern "C" fn v1(_env: JNIEnv, _this: JObject) -> u64 {
            1
        }
        extern "C" fn v2(_env: JNIEnv, _this: JObject) -> u64 {
            2
        }
        register_jni_native("com/example/Baz", "quux", "()I", v1 as *const () as usize);
        register_jni_native("com/example/Baz", "quux", "()I", v2 as *const () as usize);
        assert_eq!(
            find_jni_native("com/example/Baz", "quux", "()I"),
            Some(v2 as *const () as usize)
        );
    }

    #[test]
    fn dispatch_jni_native_no_args() {
        // Verify dispatch_jni_native calls the function and returns the result.
        extern "C" fn always_99(_env: JNIEnv, _this: JObject) -> u64 {
            99
        }
        let fn_ptr = always_99 as *const () as usize;
        let env = get_jni_env();
        let result =
            unsafe { dispatch_jni_native(fn_ptr, env, 0, &[], "(I)I") };
        // return type is 'I', raw_result = 99 → Value::Int(99)
        assert_eq!(result, crate::types::Value::Int(99));
    }

    #[test]
    fn dispatch_jni_native_void_return() {
        extern "C" fn do_nothing(_env: JNIEnv, _this: JObject) {}
        let fn_ptr = do_nothing as *const () as usize;
        let env = get_jni_env();
        let result =
            unsafe { dispatch_jni_native(fn_ptr, env, 0, &[], "()V") };
        assert_eq!(result, crate::types::Value::Object(None));
    }

    #[test]
    fn jni_register_natives_in_function_table() {
        // Index 215 must be RegisterNatives (not the stub).
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(215) };
        let stub_ptr = jni_stub as *const () as usize;
        assert_ne!(func_ptr, stub_ptr, "index 215 should be RegisterNatives, not stub");
    }

    #[test]
    fn jni_unregister_natives_in_function_table() {
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(216) };
        let stub_ptr = jni_stub as *const () as usize;
        assert_ne!(func_ptr, stub_ptr, "index 216 should be UnregisterNatives, not stub");
    }

    // -----------------------------------------------------------------------
    // M3: Global/Local reference management tests
    // -----------------------------------------------------------------------

    #[test]
    fn global_ref_handle_is_tagged() {
        // Global ref handles must have bit 0 set so jobject_to_obj can distinguish
        // them from local refs (raw heap pointers, always aligned → bit 0 = 0).
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        assert_eq!(handle & 1, 1, "global ref handle bit 0 must be set");
    }

    #[test]
    fn global_ref_jobject_to_obj_roundtrip() {
        // jobject_to_obj must correctly dereference a global ref handle.
        // NEW-11: the test previously relied on an unrelated earlier test
        // having set the JNI context TLS. In the non-synthetic default
        // build the inline `mod tests` in vm.rs is gated out, so fewer
        // tests run before this one and the TLS may be empty. The fix
        // is to set the context *and* add the ref via the shared VM's
        // global-refs table so the resolution path matches the lookup.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let obj = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let handle = shared.jni_global_refs.lock().add(obj);
        let resolved = jobject_to_obj(handle).expect("global ref must resolve to Some");
        assert_eq!(resolved, obj, "resolved global ref must match the original object");
        clear_jni_context();
    }

    #[test]
    fn global_ref_gc_roots_included() {
        // collect_roots must include objects held by global refs.
        use crate::config::VmConfig;
        use crate::memory::roots::collect_roots;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        // Create a global ref via the shared state.
        shared.jni_global_refs.lock().add(obj);
        let thread = JvmThread::new(ThreadId(0), "test");
        let roots = collect_roots(&shared, &thread);
        assert!(
            roots.contains(&obj),
            "object held by global ref must appear in GC roots"
        );
    }

    #[test]
    fn global_ref_update_after_gc() {
        // update_after_gc must update the stored ObjectRef if the object moves.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        // Simulate GC moving the object to a new address.
        let old_addr = obj.as_ptr() as usize;
        let fake_new_addr = old_addr.wrapping_add(0x100); // pretend GC moved it
        let mut pointer_map = std::collections::HashMap::new();
        pointer_map.insert(old_addr, fake_new_addr);
        refs.update_after_gc(&pointer_map);
        // The handle must now resolve to the new address.
        let updated = refs.resolve(handle).expect("handle must still be valid");
        assert_eq!(
            updated.as_ptr() as usize,
            fake_new_addr,
            "global ref must track GC movement"
        );
    }

    #[test]
    fn local_frame_push_pop() {
        // PushLocalFrame / PopLocalFrame must work correctly via the TLS stack.
        push_local_frame(8);
        track_local_ref(0x100); // fake local ref
        let result = pop_local_frame(0x200); // promote a different JObject
        assert_eq!(result, 0x200, "PopLocalFrame must return the provided result");
    }

    #[test]
    fn delete_local_ref_removes_from_frame() {
        push_local_frame(4);
        track_local_ref(0x1000);
        track_local_ref(0x2000);
        delete_local_ref(0x1000);
        // After deletion, only 0x2000 remains (verified by popping the frame).
        JNI_LOCAL_FRAMES.with(|f| {
            let stack = f.borrow();
            let top = stack.last().expect("frame must exist");
            assert!(!top.contains(&0x1000), "0x1000 must be deleted");
            assert!(top.contains(&0x2000), "0x2000 must remain");
        });
        let _ = pop_local_frame(0);
    }

    #[test]
    fn is_same_object_via_global_ref() {
        // IsSameObject must return true when comparing a local ref and a global ref
        // to the same underlying object. NEW-11: same self-inconsistency
        // fix as `global_ref_jobject_to_obj_roundtrip`.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let obj = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        let local_ref = obj_to_jobject(obj); // raw local ref
        let global_ref = shared.jni_global_refs.lock().add(obj);
        // Both refer to the same object → IsSameObject must return true.
        let resolved_local = jobject_to_obj(local_ref).unwrap();
        let resolved_global = jobject_to_obj(global_ref).unwrap();
        assert_eq!(resolved_local, resolved_global);
        clear_jni_context();
    }

    #[test]
    fn new_string_utf16_roundtrip() {
        // NewString (UTF-16) must produce the same Java String as NewStringUTF.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        let utf16: Vec<u16> = "hello".encode_utf16().collect();
        let jstr = jni_new_string(env, utf16.as_ptr(), utf16.len() as JSize);
        // Both GetStringUTFLength and GetStringLength must reflect the content.
        let utf_len = jni_get_string_utf_length(env, jstr);
        let char_len = jni_get_string_length(env, jstr);
        assert_eq!(utf_len, 5, "UTF byte length of 'hello'");
        assert_eq!(char_len, 5, "UTF-16 code unit count of 'hello'");
        clear_jni_context();
    }

    #[test]
    fn alloc_object_returns_non_null() {
        // AllocObject on a valid class must return a non-zero handle.
        let env = get_jni_env();
        // Class slot 0 is synthetic and may have 0 fields — still a valid alloc target.
        let handle = jni_alloc_object(env, 0);
        // Class 0 may not be resolvable → result is 0; just check no crash.
        let _ = handle;
    }

    #[test]
    fn set_static_boolean_byte_char_short_table_slots() {
        // Table slots 155-158 must be non-stub function pointers.
        let env = get_jni_env();
        let check = |slot: usize| {
            let func_ptr = unsafe { *(*env).add(slot) };
            assert_ne!(func_ptr, 0, "slot {slot} must not be null");
        };
        check(155); // SetStaticBooleanField
        check(156); // SetStaticByteField
        check(157); // SetStaticCharField
        check(158); // SetStaticShortField
        check(28);  // AllocObject
        check(30);  // NewObjectA
        check(163); // NewString
        check(164); // GetStringLength
        check(165); // GetStringChars
        check(166); // ReleaseStringChars
    }

    // -----------------------------------------------------------------------
    // JniGlobalRefs: add / remove / resolve / count / collect_roots / update_after_gc
    // -----------------------------------------------------------------------

    fn alloc_test_obj() -> (Arc<crate::vm::SharedVm>, ObjectRef) {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let obj = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        (shared, obj)
    }

    #[test]
    fn global_refs_resolve_returns_none_for_null_handle() {
        let refs = JniGlobalRefs::new();
        assert_eq!(refs.resolve(0), None);
    }

    #[test]
    fn global_refs_resolve_invalid_handle_not_tagged() {
        let refs = JniGlobalRefs::new();
        // A handle with bit 0 clear is not a global ref.
        assert_eq!(refs.resolve(0x1000), None);
    }

    #[test]
    fn global_refs_resolve_stale_handle() {
        let (_shared, obj) = alloc_test_obj();
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        refs.remove(handle);
        // After removal, resolve must return None.
        assert_eq!(refs.resolve(handle), None);
    }

    #[test]
    fn global_refs_count_tracks_multiple() {
        let (_shared, obj) = alloc_test_obj();
        let mut refs = JniGlobalRefs::new();
        let h1 = refs.add(obj);
        let h2 = refs.add(obj);
        assert_eq!(refs.count(), 2);
        refs.remove(h1);
        assert_eq!(refs.count(), 1);
        refs.remove(h2);
        assert_eq!(refs.count(), 0);
    }

    #[test]
    fn global_refs_collect_roots() {
        let (_shared, obj) = alloc_test_obj();
        let mut refs = JniGlobalRefs::new();
        refs.add(obj);
        refs.add(obj);
        let mut roots = Vec::new();
        refs.collect_roots(&mut roots);
        assert_eq!(roots.len(), 2);
        for r in &roots {
            assert_eq!(r.as_ptr(), obj.as_ptr());
        }
    }

    #[test]
    fn global_refs_update_after_gc() {
        let (_shared, obj) = alloc_test_obj();
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        let old_addr = obj.as_ptr() as usize;
        // Simulate GC moving the object to a new address.
        let new_addr = old_addr.wrapping_add(0x1000);
        let mut pointer_map = std::collections::HashMap::new();
        pointer_map.insert(old_addr, new_addr);
        refs.update_after_gc(&pointer_map);
        let resolved = refs.resolve(handle).expect("handle should still resolve");
        assert_eq!(resolved.as_ptr() as usize, new_addr);
    }

    #[test]
    fn global_refs_remove_returns_false_for_untagged() {
        let mut refs = JniGlobalRefs::new();
        assert!(!refs.remove(0x1000)); // bit 0 clear
    }

    // -----------------------------------------------------------------------
    // encode / decode method_id roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn method_id_roundtrip() {
        let cid = crate::classloading::ClassId::new(42);
        let method_index: u16 = 7;
        let mid = encode_method_id(cid, method_index);
        let (decoded_cid, decoded_idx) = decode_method_id(mid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, method_index);
    }

    #[test]
    fn method_id_roundtrip_zero() {
        let cid = crate::classloading::ClassId::new(0);
        let mid = encode_method_id(cid, 0);
        let (decoded_cid, decoded_idx) = decode_method_id(mid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, 0);
    }

    #[test]
    fn method_id_roundtrip_large_values() {
        let cid = crate::classloading::ClassId::new(0xFFFF_FFFF);
        let method_index: u16 = 0xFFFF;
        let mid = encode_method_id(cid, method_index);
        let (decoded_cid, decoded_idx) = decode_method_id(mid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, method_index);
    }

    // -----------------------------------------------------------------------
    // encode / decode field_id roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn field_id_roundtrip() {
        let cid = crate::classloading::ClassId::new(99);
        let field_index: usize = 5;
        let fid = encode_field_id(cid, field_index);
        let (decoded_cid, decoded_idx) = decode_field_id(fid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, field_index);
    }

    #[test]
    fn field_id_roundtrip_zero() {
        let cid = crate::classloading::ClassId::new(0);
        let fid = encode_field_id(cid, 0);
        let (decoded_cid, decoded_idx) = decode_field_id(fid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, 0);
    }

    #[test]
    fn field_id_roundtrip_large_class_id() {
        let cid = crate::classloading::ClassId::new(0xDEAD_BEEF);
        let field_index: usize = 1234;
        let fid = encode_field_id(cid, field_index);
        let (decoded_cid, decoded_idx) = decode_field_id(fid);
        assert_eq!(decoded_cid, cid);
        assert_eq!(decoded_idx, field_index);
    }

    // -----------------------------------------------------------------------
    // parse_param_types_inner
    // -----------------------------------------------------------------------

    #[test]
    fn parse_param_types_single_int() {
        // (I)V -> [b'I']
        assert_eq!(parse_param_types_inner("(I)V"), vec![b'I']);
    }

    #[test]
    fn parse_param_types_mixed() {
        // (ILjava/lang/String;[BZ)V -> [I, L, [, Z]
        let result = parse_param_types_inner("(ILjava/lang/String;[BZ)V");
        assert_eq!(result, vec![b'I', b'L', b'[', b'Z']);
    }

    #[test]
    fn parse_param_types_empty() {
        // ()V -> []
        assert_eq!(parse_param_types_inner("()V"), Vec::<u8>::new());
    }

    #[test]
    fn parse_param_types_long_double() {
        // (JD)F -> [J, D]
        assert_eq!(parse_param_types_inner("(JD)F"), vec![b'J', b'D']);
    }

    #[test]
    fn parse_param_types_arrays() {
        // ([I[Ljava/lang/Object;)V -> [[, []
        let result = parse_param_types_inner("([I[Ljava/lang/Object;)V");
        assert_eq!(result, vec![b'[', b'[']);
    }

    // -----------------------------------------------------------------------
    // parse_param_types_cached consistent with inner
    // -----------------------------------------------------------------------

    #[test]
    fn parse_param_types_cached_matches_inner() {
        let descriptors = [
            "(I)V",
            "(ILjava/lang/String;[BZ)V",
            "()V",
            "(JD)F",
            "([I[Ljava/lang/Object;)V",
        ];
        for desc in &descriptors {
            assert_eq!(
                parse_param_types_cached(desc),
                parse_param_types_inner(desc),
                "cached and inner disagree on {desc}"
            );
        }
        // Call again to exercise the cache hit path.
        for desc in &descriptors {
            assert_eq!(
                parse_param_types_cached(desc),
                parse_param_types_inner(desc),
                "cached hit disagree on {desc}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // jni_get_object_ref_type
    // -----------------------------------------------------------------------

    #[test]
    fn get_object_ref_type_null() {
        let env = get_jni_env();
        assert_eq!(jni_get_object_ref_type(env, 0), 0); // JNIInvalidRefType
    }

    #[test]
    fn get_object_ref_type_global() {
        let (_shared, obj) = alloc_test_obj();
        let mut refs = JniGlobalRefs::new();
        let handle = refs.add(obj);
        let env = get_jni_env();
        assert_eq!(jni_get_object_ref_type(env, handle), 2); // JNIGlobalRefType
    }

    #[test]
    fn get_object_ref_type_local() {
        let (_shared, obj) = alloc_test_obj();
        // Local ref is a raw pointer with bit 0 clear.
        let local = obj_to_jobject(obj);
        let env = get_jni_env();
        assert_eq!(jni_get_object_ref_type(env, local), 1); // JNILocalRefType
    }

    // --- Phase 80.2: JNI Context Arc ref-counting tests ---

    #[test]
    fn jni_context_arc_keeps_vm_alive() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let weak = Arc::downgrade(&shared);
        set_jni_context_arc(shared.clone());
        drop(shared);
        assert!(weak.upgrade().is_some(), "TLS Arc must keep SharedVm alive");
        clear_jni_context();
        assert!(weak.upgrade().is_none(), "SharedVm must be dropped after clear");
    }

    #[test]
    fn jni_context_clear_drops_arc() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        assert_eq!(Arc::strong_count(&shared), 1);
        set_jni_context_arc(shared.clone());
        assert_eq!(Arc::strong_count(&shared), 2, "TLS must hold an Arc");
        clear_jni_context();
        assert_eq!(Arc::strong_count(&shared), 1, "clear must drop TLS Arc");
    }

    // -----------------------------------------------------------------------
    // Session 44: JNI Completeness — new function tests
    // -----------------------------------------------------------------------

    #[test]
    fn jni_function_table_extended_to_234() {
        let env = get_jni_env();
        // Table must have at least 234 slots.
        // We check that slot 233 (GetModule) is not a null pointer.
        let func_ptr = unsafe { *(*env).add(233) };
        let stub_ptr = jni_stub as *const () as usize;
        assert_ne!(func_ptr, stub_ptr, "slot 233 (GetModule) should not be stub");
    }

    #[test]
    fn jni_direct_byte_buffer_roundtrip() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_jni_context_arc(shared.clone());
        let env = get_jni_env();
        // Allocate a native buffer
        let mut native_buf: Vec<u8> = vec![0u8; 1024];
        let addr = native_buf.as_mut_ptr();
        let capacity = 1024i64;
        // Create a direct byte buffer
        let buf = jni_new_direct_byte_buffer(env, addr, capacity);
        assert_ne!(buf, 0, "NewDirectByteBuffer must return non-null");
        // Get the address back
        let retrieved_addr = jni_get_direct_buffer_address(env, buf);
        assert_eq!(retrieved_addr, addr, "GetDirectBufferAddress must match");
        // Get the capacity back
        let retrieved_cap = jni_get_direct_buffer_capacity(env, buf);
        assert_eq!(retrieved_cap, capacity, "GetDirectBufferCapacity must match");
        clear_jni_context();
    }

    #[test]
    fn jni_direct_buffer_null_returns_null() {
        let env = get_jni_env();
        let buf = jni_new_direct_byte_buffer(env, std::ptr::null_mut(), 100);
        assert_eq!(buf, 0, "NewDirectByteBuffer with null address must return 0");
        let addr = jni_get_direct_buffer_address(env, 0);
        assert!(addr.is_null(), "GetDirectBufferAddress(null) must return null");
        let cap = jni_get_direct_buffer_capacity(env, 0);
        assert_eq!(cap, -1, "GetDirectBufferCapacity(null) must return -1");
    }

    #[test]
    fn jni_direct_buffer_negative_capacity() {
        let env = get_jni_env();
        let mut buf = [0u8; 16];
        let result = jni_new_direct_byte_buffer(env, buf.as_mut_ptr(), -1);
        assert_eq!(result, 0, "NewDirectByteBuffer with negative capacity must return 0");
    }

    #[test]
    fn jni_get_module_returns_null() {
        let env = get_jni_env();
        let module = jni_get_module(env, 42); // some fake class
        assert_eq!(module, 0, "GetModule must return null (unnamed module)");
    }

    #[test]
    fn jni_v_variant_slots_not_stub() {
        let env = get_jni_env();
        let stub_ptr = jni_stub as *const () as usize;
        // NewObjectV (29)
        assert_ne!(unsafe { *(*env).add(29) }, stub_ptr, "slot 29 (NewObjectV)");
        // CallObjectMethodV (35)
        assert_ne!(unsafe { *(*env).add(35) }, stub_ptr, "slot 35 (CallObjectMethodV)");
        // CallIntMethodV (50)
        assert_ne!(unsafe { *(*env).add(50) }, stub_ptr, "slot 50 (CallIntMethodV)");
        // CallVoidMethodV (62)
        assert_ne!(unsafe { *(*env).add(62) }, stub_ptr, "slot 62 (CallVoidMethodV)");
        // CallNonvirtualObjectMethodV (65)
        assert_ne!(unsafe { *(*env).add(65) }, stub_ptr, "slot 65 (CallNonvirtualObjectMethodV)");
        // CallNonvirtualVoidMethodV (92)
        assert_ne!(unsafe { *(*env).add(92) }, stub_ptr, "slot 92 (CallNonvirtualVoidMethodV)");
        // CallStaticObjectMethodV (115)
        assert_ne!(unsafe { *(*env).add(115) }, stub_ptr, "slot 115 (CallStaticObjectMethodV)");
        // CallStaticIntMethodV (130)
        assert_ne!(unsafe { *(*env).add(130) }, stub_ptr, "slot 130 (CallStaticIntMethodV)");
        // CallStaticVoidMethodV (142)
        assert_ne!(unsafe { *(*env).add(142) }, stub_ptr, "slot 142 (CallStaticVoidMethodV)");
    }

    #[test]
    fn jni_nio_slots_not_stub() {
        let env = get_jni_env();
        let stub_ptr = jni_stub as *const () as usize;
        assert_ne!(unsafe { *(*env).add(229) }, stub_ptr, "slot 229 (NewDirectByteBuffer)");
        assert_ne!(unsafe { *(*env).add(230) }, stub_ptr, "slot 230 (GetDirectBufferAddress)");
        assert_ne!(unsafe { *(*env).add(231) }, stub_ptr, "slot 231 (GetDirectBufferCapacity)");
        assert_ne!(unsafe { *(*env).add(232) }, stub_ptr, "slot 232 (GetObjectRefType)");
        assert_ne!(unsafe { *(*env).add(233) }, stub_ptr, "slot 233 (GetModule)");
    }

    #[test]
    fn jni_get_object_ref_type_at_slot_232() {
        let env = get_jni_env();
        let func_ptr = unsafe { *(*env).add(232) };
        let get_ref_type: extern "C" fn(JNIEnv, JObject) -> JInt =
            unsafe { std::mem::transmute(func_ptr) };
        // null → JNIInvalidRefType = 0
        assert_eq!(get_ref_type(env, 0), 0);
    }

    #[test]
    fn jni_count_non_stub_functions() {
        // Verify that we have at least 165 non-stub functions (was ~129, now ~165+).
        let env = get_jni_env();
        let stub_ptr = jni_stub as *const () as usize;
        let mut non_stub_count = 0;
        for i in 0..JNI_FUNCTION_COUNT {
            let ptr = unsafe { *(*env).add(i) };
            if ptr != stub_ptr {
                non_stub_count += 1;
            }
        }
        assert!(
            non_stub_count >= 165,
            "Expected at least 165 non-stub JNI functions, got {non_stub_count}"
        );
    }

    #[test]
    fn jni_va_list_to_jvalues_null_mid() {
        // null method ID → returns null pointer
        let (ptr, len) = va_list_to_jvalues(0, std::ptr::null_mut());
        assert!(ptr.is_null());
        assert_eq!(len, 0);
    }

    #[test]
    fn jni_invoke_table_attach_detach() {
        let vm = get_java_vm();
        // AttachCurrentThread (slot 4)
        let func_ptr = unsafe { *(*vm).add(4) };
        assert_ne!(func_ptr, 0, "AttachCurrentThread must be registered");
        // DetachCurrentThread (slot 5)
        let func_ptr = unsafe { *(*vm).add(5) };
        assert_ne!(func_ptr, 0, "DetachCurrentThread must be registered");
        // AttachCurrentThreadAsDaemon (slot 7)
        let func_ptr = unsafe { *(*vm).add(7) };
        assert_ne!(func_ptr, 0, "AttachCurrentThreadAsDaemon must be registered");
    }

}
