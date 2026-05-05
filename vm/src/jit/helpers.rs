//! JIT runtime helper functions — called from JIT-compiled code via absolute CALL.
//!
//! These functions need access to `SharedVm` and other VM internals, so they
//! live in the VM crate rather than the standalone JIT crate.

use std::cell::Cell;

use rustjvm_jit::{
    DescriptorParamIter, JitInvokeInfo, JitMICSlot, JitRuntimeHelpers,
};
use rustjvm_types::{
    ArrayElementType, ClassId, ObjectRef, Value,
    ARRAY_LENGTH_OFFSET, HEADER_SIZE, REF_ELEMENT_SIZE, SLOT_SIZE,
};

use crate::memory::vm_heap::VmHeap;
use crate::threading::jvm_thread::JvmThread;
use crate::vm::SharedVm;

// ---------------------------------------------------------------------------
// Thread-local JvmThread pointer for JIT helper access
// ---------------------------------------------------------------------------

thread_local! {
    /// Stores a raw pointer to the current thread's JvmThread.
    /// Safety invariant: only ONE `&mut JvmThread` is derived from this at a time,
    /// and only within a single JIT helper call scope. The pointer is set before
    /// entering JIT code and cleared immediately after.
    static JIT_THREAD: Cell<*mut JvmThread> = const { Cell::new(std::ptr::null_mut()) };

    /// Pending Java exception from JIT dispatch. When `jit_invoke_dispatch` calls
    /// a method that throws, we store the exception here instead of swallowing it.
    /// The interpreter checks this after JIT code returns and propagates it through
    /// the normal exception handling path (exception tables, frame unwinding).
    static JIT_PENDING_EXCEPTION: Cell<Option<ObjectRef>> = const { Cell::new(None) };

    /// Pending AIOOBE from JIT bounds check.  Set by `jit_throw_aioobe`,
    /// consumed by the interpreter after JIT code returns `i64::MIN`.
    static JIT_PENDING_AIOOBE: Cell<Option<(i64, i64)>> = const { Cell::new(None) };
}

/// Set the current thread's JvmThread pointer for JIT helper access.
/// Returns the previously stored pointer so callers can restore it later
/// (re-entrant JIT calls via interpreter::execute inside jit_invoke_dispatch).
///
/// # Safety contract
/// The caller must ensure that no other `&mut JvmThread` reference exists for the
/// duration of JIT execution. The pointer is only dereferenced inside JIT helpers
/// which execute on the same thread that set it.
pub fn set_jit_thread(thread: &mut JvmThread) -> *mut JvmThread {
    JIT_THREAD.with(|t| {
        let old = t.get();
        t.set(thread as *mut JvmThread);
        old
    })
}

/// Check if the JIT thread pointer is already set.
#[inline(always)]
pub fn is_jit_thread_set() -> bool {
    JIT_THREAD.with(|t| !t.get().is_null())
}

/// Restore a previously saved JIT thread pointer. Used to support re-entrant
/// JIT calls (e.g. JIT put() → jit_invoke_dispatch → interpreter::execute hash()
/// which may JIT-compile hash() and call set_jit_thread again).
pub fn restore_jit_thread(old: *mut JvmThread) {
    JIT_THREAD.with(|t| t.set(old));
}

/// Clear the JIT thread pointer after JIT execution completes.
pub fn clear_jit_thread() {
    JIT_THREAD.with(|t| t.set(std::ptr::null_mut()));
}

/// Store a pending Java exception from JIT dispatch. Called when
/// `jit_invoke_dispatch` encounters an `ExceptionThrown` error.
fn set_jit_pending_exception(exc: ObjectRef) {
    JIT_PENDING_EXCEPTION.with(|e| e.set(Some(exc)));
}

/// Take (consume) any pending Java exception set by JIT dispatch.
/// Returns `Some(ObjectRef)` if an exception was pending, `None` otherwise.
pub fn take_jit_pending_exception() -> Option<ObjectRef> {
    JIT_PENDING_EXCEPTION.with(|e| e.take())
}

/// Take (consume) a pending AIOOBE from JIT bounds check.
/// Returns `Some((index, length))` if an AIOOBE was pending.
pub fn take_jit_pending_aioobe() -> Option<(i64, i64)> {
    JIT_PENDING_AIOOBE.with(|e| e.take())
}

/// Obtain an exclusive reference to the JIT thread. Returns None if not set.
///
/// # Safety
/// Caller must ensure this is only called from JIT helper functions on the same
/// thread that called `set_jit_thread`, and that no other reference to the
/// JvmThread is live.
// SAFETY: Caller must ensure this is only called from JIT helper functions on the
// same thread that called `set_jit_thread`, and that no other `&mut JvmThread`
// reference is live. The pointer was set by `set_jit_thread` from a valid `&mut JvmThread`.
#[inline]
unsafe fn jit_thread_mut() -> Option<&'static mut JvmThread> {
    let ptr = JIT_THREAD.with(|t| t.get());
    if ptr.is_null() {
        None
    } else {
        Some(&mut *ptr)
    }
}

// ---------------------------------------------------------------------------
// Helper: extract heap from SharedVm pointer
// ---------------------------------------------------------------------------

// SAFETY: Caller must ensure vm_ptr is a valid pointer to a live SharedVm instance.
// The SharedVm is heap-allocated and outlives all JIT helper calls.
#[inline]
unsafe fn heap_from_vm(vm_ptr: i64) -> &'static VmHeap {
    debug_assert!(vm_ptr != 0, "heap_from_vm called with null VM pointer");
    // SAFETY: vm_ptr was passed from JIT-compiled code which received it from the
    // interpreter's SharedVm reference, so it points to a valid SharedVm.
    &(*(vm_ptr as *const SharedVm)).heap
}

// ---------------------------------------------------------------------------
// Array allocation helpers
// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer
// passed through from the interpreter. atype encodes a JVM array element type (T_BOOLEAN..T_LONG).
// length is the requested array size. The returned i64 is a raw heap pointer to the new array.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_newarray(vm_ptr: i64, atype: i64, length: i64) -> i64 {
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let heap = &vm.heap;
    let elem_type = match atype as u8 {
        4 => ArrayElementType::Boolean,
        5 => ArrayElementType::Char,
        6 => ArrayElementType::Float,
        7 => ArrayElementType::Double,
        8 => ArrayElementType::Byte,
        9 => ArrayElementType::Short,
        10 => ArrayElementType::Int,
        11 => ArrayElementType::Long,
        _ => return 0,
    };
    // Try allocation; if young gen exhausted, run GC and retry
    let data_size = rustjvm_types::array_data_size(length as usize, elem_type).unwrap_or(0);
    let total_size = rustjvm_types::HEADER_SIZE + data_size;
    if heap.try_alloc_young_probe(total_size).is_none() {
        // Young gen full — trigger GC from JIT context
        if let Some(thread) = jit_thread_mut() {
            let mut roots = crate::memory::roots::collect_roots(vm, thread);
            let result = heap.collect_garbage(&mut roots, &vm.monitors);
            crate::memory::gc::update_all_roots(vm, thread, &result.pointer_map);
        }
    }
    let obj_ref = heap.alloc_array(ClassId::new(0), elem_type, length as usize);
    obj_ref.as_ptr() as i64
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and num_fields must match the class metadata resolved at compile time.
// The returned i64 is a raw heap pointer to the newly allocated object.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_new_object(vm_ptr: i64, class_id_raw: i64, num_fields: i64) -> i64 {
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let heap = &vm.heap;
    let class_id = ClassId::new(class_id_raw as u32);
    let obj_ref = heap.alloc_object(class_id, num_fields as usize);
    // Initialize primitive-typed fields to proper JVM default values.
    // Zero memory reads as Object(None) which is wrong for int/long/float/double fields.
    jit_init_primitive_fields(vm, obj_ref, class_id);
    // Register with GC finalizer support if the class overrides finalize() (JLS §12.6).
    let has_fin = vm
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .map_or(false, |c| c.has_finalizer);
    if has_fin {
        vm.register_finalizable(obj_ref.as_ptr() as usize);
    }
    obj_ref.as_ptr() as i64
}

/// Initialize primitive-typed fields of a newly allocated object (JIT version).
fn jit_init_primitive_fields(vm: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    let cm = vm.class_manager.read();
    let store = &cm.class_store;
    let mut cid = Some(class_id);
    while let Some(current_id) = cid {
        if let Some(class) = store.get(current_id) {
            let mut inst_idx = class.first_field_index;
            for f in &class.fields {
                if f.is_static() { continue; }
                let desc_first = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                let default = match desc_first {
                    b'I' | b'B' | b'C' | b'S' | b'Z' => Some(Value::Int(0)),
                    b'J' => Some(Value::Long(0)),
                    b'F' => Some(Value::Float(0.0)),
                    b'D' => Some(Value::Double(0.0)),
                    _ => None,
                };
                if let Some(val) = default {
                    vm.heap.set_field(obj, inst_idx, val);
                }
                inst_idx += 1;
            }
            cid = class.superclass;
        } else {
            break;
        }
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// component_class_id_raw is the ClassId of the array's component type. length is non-negative.
// Returns a raw heap pointer to a newly allocated reference array.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_anewarray_object(
    vm_ptr: i64,
    component_class_id_raw: i64,
    length: i64,
) -> i64 {
    let heap = heap_from_vm(vm_ptr);
    let class_id = ClassId::new(component_class_id_raw as u32);
    let arr = heap.alloc_array(class_id, ArrayElementType::Reference, length as usize);
    arr.as_ptr() as i64
}

// ---------------------------------------------------------------------------
// Array element access helpers
// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to a byte/boolean array object. Null and out-of-bounds are handled gracefully.
pub unsafe extern "C" fn jit_baload(array_ptr: i64, index: i64) -> i64 {
    if array_ptr == 0 { return 0; }
    // SAFETY: array_ptr is non-null and points to a live array object on the GC heap.
    // ARRAY_LENGTH_OFFSET is the fixed offset to the length field in the array header.
    let ptr = array_ptr as *const u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        return 0;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize);
    *elem_ptr as i8 as i64
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to a byte/boolean array object. Null and out-of-bounds are handled gracefully.
pub unsafe extern "C" fn jit_bastore(array_ptr: i64, index: i64, val: i64) {
    if array_ptr == 0 { return; }
    // SAFETY: array_ptr is non-null and points to a live array object on the GC heap.
    let ptr = array_ptr as *mut u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        return;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize);
    *elem_ptr = val as u8;
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to an int array object. Null and out-of-bounds are handled gracefully.
pub unsafe extern "C" fn jit_iaload(array_ptr: i64, index: i64) -> i64 {
    if array_ptr == 0 { return 0; }
    // SAFETY: array_ptr is non-null and points to a live int[] on the GC heap.
    // The element at HEADER_SIZE + index*4 is within bounds (checked below).
    let ptr = array_ptr as *const u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        return 0;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * 4) as *const i32;
    *elem_ptr as i64
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to an int array object. Null and out-of-bounds are handled gracefully.
pub unsafe extern "C" fn jit_iastore(array_ptr: i64, index: i64, val: i64) {
    if array_ptr == 0 { return; }
    // SAFETY: array_ptr is non-null and points to a live int[] on the GC heap.
    let ptr = array_ptr as *mut u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        return;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * 4) as *mut i32;
    *elem_ptr = val as i32;
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to a reference array object. Null and out-of-bounds are handled gracefully.
pub unsafe extern "C" fn jit_aaload(array_ptr: i64, index: i64) -> i64 {
    if array_ptr == 0 { return 0; }
    // SAFETY: array_ptr is non-null and points to a live Object[] on the GC heap.
    // ptr::read is used because Value::Object may contain non-Copy ObjectRef.
    let ptr = array_ptr as *const u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        return 0;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * REF_ELEMENT_SIZE) as *const u64;
    std::ptr::read(elem_ptr) as i64
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// array_ptr must be 0 (null) or a valid heap pointer to a reference array.
// val is 0 (null) or a raw pointer to a live heap object. Write barrier is issued
// for non-null stores to maintain generational GC card table invariants.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_aastore(vm_ptr: i64, array_ptr: i64, index: i64, val: i64) {
    if array_ptr == 0 { return; }
    // SAFETY: array_ptr is non-null and points to a live Object[] on the GC heap.
    let ptr = array_ptr as *mut u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        return;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * REF_ELEMENT_SIZE) as *mut u64;
    std::ptr::write(elem_ptr, val as u64);

    if val != 0 {
        let heap = heap_from_vm(vm_ptr);
        let obj_ref = ObjectRef::from_raw(ptr);
        let value = Value::Object(Some(ObjectRef::from_raw(val as usize as *mut u8)));
        heap.write_barrier(obj_ref, value);
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// leaf_et encodes the inner array's element type. dim1 and dim2 are the two dimension sizes.
// Returns a raw heap pointer to the outer reference array whose elements are inner arrays.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_multianewarray_2d(
    vm_ptr: i64,
    leaf_et: i64,
    dim1: i64,
    dim2: i64,
) -> i64 {
    let heap = heap_from_vm(vm_ptr);
    let elem_type = match leaf_et as u8 {
        4 => ArrayElementType::Boolean,
        5 => ArrayElementType::Char,
        6 => ArrayElementType::Float,
        7 => ArrayElementType::Double,
        8 => ArrayElementType::Byte,
        9 => ArrayElementType::Short,
        10 => ArrayElementType::Int,
        11 => ArrayElementType::Long,
        _ => ArrayElementType::Reference,
    };

    let outer = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, dim1 as usize);
    for i in 0..dim1 as usize {
        let inner = heap.alloc_array(ClassId::new(0), elem_type, dim2 as usize);
        let _ = heap.set_array_element(outer, i, Value::Object(Some(inner)));
    }
    outer.as_ptr() as i64
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to any array object. Returns the array length or -1 for null.
pub unsafe extern "C" fn jit_arraylength(array_ptr: i64) -> i64 {
    if array_ptr == 0 { return -1; }
    // SAFETY: array_ptr is non-null and points to a live array on the GC heap.
    // ARRAY_LENGTH_OFFSET is the fixed offset to the u32 length field.
    let ptr = array_ptr as *const u8;
    let length_ptr = ptr.add(ARRAY_LENGTH_OFFSET) as *const u32;
    (*length_ptr) as i64
}

// ---------------------------------------------------------------------------
// Field access helpers
// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index is the resolved field slot index within the object layout.
// ptr::read is used because Value may contain non-Copy variants (ObjectRef).
pub unsafe extern "C" fn jit_getfield(obj_ptr: i64, field_index: i64) -> i64 {
    if obj_ptr == 0 { return 0; }
    // SAFETY: obj_ptr is non-null and points to a live object. HEADER_SIZE + field_index * SLOT_SIZE
    // is within the object's allocated region because field_index was resolved at JIT compile time.
    let ptr = (obj_ptr as *const u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    let val: Value = std::ptr::read(ptr as *const Value);
    let result = match val {
        Value::Int(i) => i as i64,
        Value::Long(l) => l,
        Value::Float(f) => f.to_bits() as i64,
        Value::Double(d) => d.to_bits() as i64,
        Value::Object(Some(r)) => r.as_ptr() as i64,
        Value::Object(None) => 0,
        _ => 0,
    };
    result
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_int(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    std::ptr::write(ptr as *mut Value, Value::Int(val as i32));
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_long(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    std::ptr::write(ptr as *mut Value, Value::Long(val));
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_float(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    std::ptr::write(ptr as *mut Value, Value::Float(f32::from_bits(val as u32)));
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_double(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    std::ptr::write(ptr as *mut Value, Value::Double(f64::from_bits(val as u64)));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr must be 0 (null) or a valid heap pointer to a live object.
// val is 0 (null) or a raw pointer to a live heap object. Write barrier is issued
// for non-null stores to maintain generational GC invariants.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putfield_object(
    vm_ptr: i64,
    obj_ptr: i64,
    field_index: i64,
    val: i64,
) {
    if obj_ptr == 0 { return; }
    let obj_ref = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    let value = if val == 0 {
        Value::Object(None)
    } else {
        Value::Object(Some(ObjectRef::from_raw(val as usize as *mut u8)))
    };
    let ptr = obj_ref
        .as_ptr()
        .add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    std::ptr::write(ptr as *mut Value, value);
    if val != 0 {
        let heap = heap_from_vm(vm_ptr);
        heap.write_barrier(obj_ref, value);
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr and val_ptr must be 0 (null) or valid heap pointers to live objects.
// Records a generational write barrier so the GC tracks old-to-young references.
pub unsafe extern "C" fn jit_write_barrier(vm_ptr: i64, obj_ptr: i64, val_ptr: i64) {
    if obj_ptr == 0 { return; }
    if val_ptr == 0 {
        return;
    }
    let heap = heap_from_vm(vm_ptr);
    let obj_ref = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    let val_ref = ObjectRef::from_raw(val_ptr as usize as *mut u8);
    let value = Value::Object(Some(val_ref));
    heap.write_barrier(obj_ref, value);
}

// ---------------------------------------------------------------------------
// Static field helpers
// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_getstatic(vm_ptr: i64, class_id_raw: i64, field_index: i64) -> i64 {
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    let val = crate::vm::get_static_shared(vm, class_id, field_index as usize);
    match val {
        Value::Int(i) => i as i64,
        Value::Long(l) => l,
        Value::Float(f) => f.to_bits() as i64,
        Value::Double(d) => d.to_bits() as i64,
        Value::Object(Some(r)) => r.as_ptr() as i64,
        Value::Object(None) => 0,
        _ => 0,
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_int(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    if vm_ptr == 0 {
        return;
    }
    // SAFETY: vm_ptr is non-null and points to a valid SharedVm.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Int(val as i32));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_long(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Long(val));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_float(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Float(f32::from_bits(val as u32)));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_double(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Double(f64::from_bits(val as u64)));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time. val is 0 (null) or a raw
// pointer to a live heap object, converted to Value::Object for storage in the static field table.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_object(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    let value = if val == 0 {
        Value::Object(None)
    } else {
        Value::Object(Some(ObjectRef::from_raw(val as usize as *mut u8)))
    };
    crate::vm::set_static_shared(vm, class_id, field_index as usize, value);
}

// ---------------------------------------------------------------------------
// Type check helpers
// ---------------------------------------------------------------------------

/// Common type-check resolution shared by `jit_checkcast` and `jit_instanceof`.
///
/// Returns `true` if `obj_ref` (which must be non-null and live) is an instance
/// of the class named `class_name`. Mirrors the interpreter's `Instanceof` /
/// `Checkcast` semantics exactly:
///
/// 1. Resolve the target class via `load_class_concurrent` so a not-yet-loaded
///    target class is loaded on demand. This is the fix for the long-standing
///    JIT instanceof miscompile that returned `false` whenever the target class
///    happened to be loaded only after the JIT call site warmed up.
/// 2. Fall back to `lambda_proxy_satisfies` for objects whose class id is a
///    synthetic lambda proxy (>= 0x8000_0000 — never present in `class_store`).
/// 3. Fall back to `synthetic_implements` for the hand-built collection helper
///    classes whose interface relationships live in `synthetic_implements`
///    rather than in the loaded class hierarchy.
///
/// # Safety
/// Caller must ensure `vm_ptr` is a valid `SharedVm` pointer and `obj_ref` is
/// derived from a live heap object (or that the caller has already short-circuited
/// the null case). The function holds only short-lived `class_manager.read()` /
/// `class_manager.write()` locks and never reborrows the heap.
// SAFETY: Caller must ensure vm_ptr (via `vm`) is a valid SharedVm reference and obj_ref is
// derived from a live heap object. Only short-lived class_manager read/write locks are held;
// the heap is never reborrowed. The null case must be handled by the caller before entry.
unsafe fn jit_typecheck_resolve(
    vm: &SharedVm,
    obj_class_id: ClassId,
    obj_ref: ObjectRef,
    class_name: &str,
) -> bool {
    // Fast path: target already loaded. Most call sites hit this.
    //
    // IMPORTANT: bind the result to a local so the `RwLockReadGuard` temporary
    // from `.read()` is dropped at the semicolon. Using `if let Some(x) =
    // rwlock.read().method()` would extend the guard's lifetime to the entire
    // `if let` block (including the `else` branch), deadlocking any path that
    // later calls `load_class_concurrent` (which needs a write lock).
    let target_class_id_opt = vm.class_manager.read().find_class_by_name(class_name);
    if let Some(target_class_id) = target_class_id_opt {
        let is_subclass = vm
            .class_manager
            .read()
            .is_subclass_of(obj_class_id, target_class_id);
        if is_subclass {
            return true;
        }
        // Lambda proxy fallback uses the *already-resolved* target id.
        if crate::runtime::interpreter::lambda_proxy_satisfies_public(
            vm,
            obj_class_id,
            target_class_id,
        ) {
            return true;
        }
    } else {
        // Slow path: target not yet loaded. Load it on demand using the
        // concurrent loader so we don't deadlock if another thread is racing
        // the same load. Failure is silently treated as "not assignable",
        // matching what HotSpot does for unresolvable targets in instanceof
        // (instanceof on an unresolvable target returns false; checkcast
        // would have been linked earlier and is a different failure mode).
        if let Ok(target_class_id) = vm.load_class_concurrent(class_name) {
            let is_subclass = vm
                .class_manager
                .read()
                .is_subclass_of(obj_class_id, target_class_id);
            if is_subclass {
                return true;
            }
            if crate::runtime::interpreter::lambda_proxy_satisfies_public(
                vm,
                obj_class_id,
                target_class_id,
            ) {
                return true;
            }
        }
    }

    // Name-based fallback for synthetic classes whose interface relationships
    // are encoded in `synthetic_implements` rather than in the class hierarchy.
    if crate::runtime::interpreter::synthetic_implements_public(vm, obj_class_id, class_name) {
        return true;
    }

    // Array fallback: arrays with class_id 0 (e.g. from Array.newInstance via JIT)
    // lack class hierarchy entries.  Any reference array is assignable to
    // [Ljava/lang/Object; and any array is assignable to java/lang/Object,
    // java/io/Serializable, or java/lang/Cloneable.
    if vm.heap.kind_of(obj_ref) == rustjvm_types::ObjectKind::Array {
        if class_name == "[Ljava/lang/Object;"
            || class_name == "java/lang/Object"
            || class_name == "java/io/Serializable"
            || class_name == "java/lang/Cloneable"
        {
            return true;
        }
    }

    let _ = obj_ref;
    false
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr is 0 (null) or a valid heap pointer. class_name_ptr/class_name_len form a
// valid UTF-8 slice pointing into the JIT-compiled code's string table (or are null/<=0
// for an unresolved site, which fails closed). Returns obj_ptr on success, 0 on failure.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_checkcast(
    vm_ptr: i64,
    obj_ptr: i64,
    class_name_ptr: *const u8,
    class_name_len: i64,
) -> i64 {
    // Null reference is always a valid cast (matches JVMS §6.5.checkcast).
    if obj_ptr == 0 {
        return 0;
    }
    // Defensive: an unresolved typecheck site (no class_name attached) must
    // not silently allow the cast. Return 0 so the JIT-compiled code observes
    // a "failed cast" and falls back to the interpreter exception path.
    if class_name_len <= 0 || class_name_ptr.is_null() {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    // SAFETY: class_name_ptr is non-null (checked above) and class_name_len > 0.
    // The pointer comes from the JIT string table which outlives this call.
    let class_name = match std::str::from_utf8(std::slice::from_raw_parts(
        class_name_ptr,
        class_name_len as usize,
    )) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    // SAFETY: obj_ptr is non-null (checked above) and points to a live heap object.
    let obj_ref = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    let obj_class_id = vm.heap.class_id_of(obj_ref);
    if jit_typecheck_resolve(vm, obj_class_id, obj_ref, class_name) {
        obj_ptr
    } else {
        0
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr is 0 (null) or a valid heap pointer. class_name_ptr/class_name_len form a
// valid UTF-8 slice pointing into the JIT string table (or are null/<=0 for unresolved,
// which returns 0). Returns 1 if obj is an instance, 0 otherwise.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_instanceof(
    vm_ptr: i64,
    obj_ptr: i64,
    class_name_ptr: *const u8,
    class_name_len: i64,
) -> i64 {
    // Null reference is never an instance of anything (JVMS §6.5.instanceof).
    if obj_ptr == 0 {
        return 0;
    }
    if class_name_len <= 0 || class_name_ptr.is_null() {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    // SAFETY: class_name_ptr is non-null (checked above) and class_name_len > 0.
    // The pointer comes from the JIT string table which outlives this call.
    let class_name = match std::str::from_utf8(std::slice::from_raw_parts(
        class_name_ptr,
        class_name_len as usize,
    )) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    // SAFETY: obj_ptr is non-null (checked above) and points to a live heap object.
    let obj_ref = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    let obj_class_id = vm.heap.class_id_of(obj_ref);
    if jit_typecheck_resolve(vm, obj_class_id, obj_ref, class_name) {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Bounds check helper
// ---------------------------------------------------------------------------

/// JIT bounds-check helper — sets a pending AIOOBE flag and returns `i64::MIN`
/// (the deopt sentinel) to signal the interpreter that a bounds check failed.
///
/// On Windows, JIT frames have no SEH unwind tables, so panicking here would
/// terminate the process instead of unwinding to the `catch_unwind` in the
/// interpreter.  Using a thread-local flag sidesteps this platform limitation.
// SAFETY: Called from JIT-compiled code when an array bounds check fails.
// Only stores two i64 values in a thread-local; no pointer dereferences.
pub unsafe extern "C" fn jit_throw_aioobe(index: i64, length: i64) -> i64 {
    JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))));
    i64::MIN // deopt sentinel — interpreter will detect and throw AIOOBE
}

// ---------------------------------------------------------------------------
// Invoke dispatch helpers
// ---------------------------------------------------------------------------

/// Per-info-pointer JIT dispatch state: caches a compiled callee's entry point
/// so that repeated calls from the same JIT call site skip the JIT cache lookup.
struct DispatchCache {
    entry: usize,
    needs_context: bool,
}

// Thread-local map from JitInvokeInfo pointer -> cached JIT entry.
// Using a thread-local avoids synchronization on the hot path.
// T10.9.B: FxHashMap — pointer values are internal; this is touched on every
// JIT-dispatched invoke.
thread_local! {
    static DISPATCH_CACHE: std::cell::RefCell<rustc_hash::FxHashMap<usize, DispatchCache>>
        = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    static DISPATCH_COUNTER: std::cell::RefCell<rustc_hash::FxHashMap<usize, u32>>
        = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Invocation threshold for triggering JIT compilation from the dispatch helper.
const DISPATCH_JIT_THRESHOLD: u32 = 500;

/// S112r9 — JIT dispatch error handler. When a JIT-dispatched callee returns
/// an error, route it through `JIT_PENDING_EXCEPTION` so the interpreter's
/// post-JIT exception-routing path can find a handler (or propagate to the
/// top of the JVM with a printable message).
///
/// Previously only `MethodCallFailed::ExceptionThrown` was captured, and
/// `MethodCallFailed::InternalError` was silently dropped — the JIT helper
/// returned 0/null to the JIT caller, which would proceed as if the call
/// returned a benign null. That was the root cause of Spring Boot 3 fat-jars
/// exiting silently with rc=0 between `prepareEnvironment` and `printBanner`:
/// some downstream invoke produced an `InternalError` ("method has no Code
/// attribute" or similar linkage gap), the JIT swallowed it, the JIT'd
/// `prepareEnvironment` continued with corrupt state and returned, then the
/// caller `run()` returned cleanly without ever reaching `printBanner`.
///
/// Wrapping the InternalError in a Java `java/lang/InternalError` gives the
/// VM a real Throwable to walk through exception tables. If the heap is
/// exhausted or the class can't be loaded, we fall back to leaving the
/// error unstored — the original "swallow and return 0" behaviour. That
/// keeps this purely additive: it never makes a previously-working scenario
/// worse, only converts silent rc=0 into a visible stack trace.
fn handle_jit_dispatch_error(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
    info: &JitInvokeInfo,
) {
    use crate::error::MethodCallFailed;
    match err {
        MethodCallFailed::ExceptionThrown(exc) => {
            set_jit_pending_exception(exc);
        }
        MethodCallFailed::InternalError(vm_err) => {
            // Format a message that points at the failing dispatch site so
            // the user can see WHICH callee blew up. This is the difference
            // between a silent rc=0 and a visible "Exception in thread main"
            // for Spring Boot.
            let msg = format!(
                "JIT dispatch into {}.{}{} failed: {}",
                info.class_name, info.method_name, info.descriptor, vm_err,
            );
            // Try to wrap in a Java `InternalError`; on any allocation /
            // load failure, fall through to the legacy silent drop so we
            // never make things worse than before this fix.
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm, thread, "java/lang/InternalError", Some(&msg),
            ) {
                set_jit_pending_exception(exc);
            }
        }
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// info_ptr must point to a live JitInvokeInfo (heap-allocated, outlives this call).
// args_ptr/num_args form a valid i64 slice of JIT-encoded arguments.
// Transmutes within this function convert cached JIT entry pointers to function pointers
// with known signatures matching the compiled method's calling convention.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn jit_invoke_dispatch(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    // SAFETY: vm_ptr and info_ptr originate from JIT code; both point to valid, live objects.
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    if std::env::var_os("RUSTJVM_DBG_JIT_DISPATCH").is_some() {
        let p = args_ptr as *const i64;
        let mut buf = String::new();
        if !p.is_null() && num_args > 0 {
            for i in 0..(num_args as usize).min(4) {
                let v = unsafe { *p.add(i) };
                buf.push_str(&format!(" arg{}=0x{:x}", i, v));
            }
        }
        eprintln!(
            "[JIT_DISPATCH] {}.{}{} kind={} num_args={}{}",
            info.class_name, info.method_name, info.descriptor, info.invoke_kind, num_args, buf,
        );
    }
    if num_args < 0 || (num_args > 0 && (args_ptr as *const i64).is_null()) {
        return 0;
    }
    // SAFETY: args_ptr is non-null (checked above) and num_args >= 0.
    // The JIT caller allocated this array on its own stack frame.
    let args_slice = if num_args == 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };

    // Fast path: check thread-local dispatch cache for a previously-compiled callee.
    // This avoids the JIT cache lock on every call.
    let info_key = info_ptr as usize;
    let cached_entry = DISPATCH_CACHE.with(|dc| {
        dc.borrow().get(&info_key).map(|c| (c.entry, c.needs_context))
    });
    if let Some((entry, needs_ctx)) = cached_entry {
        // SAFETY: entry is a JIT-compiled function pointer cached from a previous successful
        // compilation. The transmutes convert it to the correct extern "C" fn signature based
        // on arg count. The JIT compiler guarantees the compiled code uses extern "C" ABI with
        // i64 parameters matching the method's JVM descriptor.
        let n = num_args as usize;
        let cached_ret = if needs_ctx {
            match n {
                0 => {
                    let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                    f(vm_ptr)
                }
                1 => {
                    let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                    f(vm_ptr, args_slice[0])
                }
                2 => {
                    let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                    f(vm_ptr, args_slice[0], args_slice[1])
                }
                3 => {
                    let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                    f(vm_ptr, args_slice[0], args_slice[1], args_slice[2])
                }
                _ => 0,
            }
        } else {
            match n {
                0 => {
                    let f: unsafe extern "C" fn() -> i64 = std::mem::transmute(entry);
                    f()
                }
                1 => {
                    let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                    f(args_slice[0])
                }
                2 => {
                    let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                    f(args_slice[0], args_slice[1])
                }
                3 => {
                    let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                    f(args_slice[0], args_slice[1], args_slice[2])
                }
                4 => {
                    let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                    f(args_slice[0], args_slice[1], args_slice[2], args_slice[3])
                }
                _ => 0,
            }
        };
        return cached_ret;
    }

    // Check JIT cache for a compiled version of this callee
    {
        let class_arc: std::sync::Arc<str> = std::sync::Arc::from(info.class_name);
        let method_arc: std::sync::Arc<str> = std::sync::Arc::from(info.method_name);
        let desc_arc: std::sync::Arc<str> = std::sync::Arc::from(info.descriptor);
        let jit_cache = vm.jit_cache.read();
        if let Some(compiled) = jit_cache.get(&class_arc, &method_arc, &desc_arc) {
            let entry = compiled.entry_ptr() as usize;
            let needs_ctx = compiled.needs_context();
            // Cache for future calls
            DISPATCH_CACHE.with(|dc| {
                dc.borrow_mut().insert(info_key, DispatchCache { entry, needs_context: needs_ctx });
            });
            drop(jit_cache);
            // SAFETY: entry was obtained from a CompiledMethod in the JIT cache, whose
            // entry_ptr points to executable memory with the correct extern "C" ABI.
            // The transmutes match the compiled method's parameter count.
            let n = num_args as usize;
            if needs_ctx {
                return match n {
                    0 => {
                        let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                        f(vm_ptr)
                    }
                    1 => {
                        let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                        f(vm_ptr, args_slice[0])
                    }
                    2 => {
                        let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                        f(vm_ptr, args_slice[0], args_slice[1])
                    }
                    3 => {
                        let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                        f(vm_ptr, args_slice[0], args_slice[1], args_slice[2])
                    }
                    _ => 0,
                };
            } else {
                return match n {
                    0 => {
                        let f: unsafe extern "C" fn() -> i64 = std::mem::transmute(entry);
                        f()
                    }
                    1 => {
                        let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                        f(args_slice[0])
                    }
                    2 => {
                        let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                        f(args_slice[0], args_slice[1])
                    }
                    3 => {
                        let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                        f(args_slice[0], args_slice[1], args_slice[2])
                    }
                    4 => {
                        let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                        f(args_slice[0], args_slice[1], args_slice[2], args_slice[3])
                    }
                    _ => 0,
                };
            }
        }
    }

    // Invocation counting — trigger compilation for hot callees
    let should_compile = DISPATCH_COUNTER.with(|dc| {
        let mut map = dc.borrow_mut();
        let count = map.entry(info_key).or_insert(0);
        *count += 1;
        *count == DISPATCH_JIT_THRESHOLD
    });
    if should_compile {
        // Try to compile the callee and cache it
        if let Some((entry, needs_ctx)) = try_compile_callee(vm, info) {
            DISPATCH_CACHE.with(|dc| {
                dc.borrow_mut().insert(info_key, DispatchCache { entry, needs_context: needs_ctx });
            });
            // SAFETY: entry was just produced by try_compile_callee, which returns a validated
            // JIT entry pointer. The transmutes match the compiled method's parameter count.
            let n = num_args as usize;
            if needs_ctx {
                return match n {
                    0 => {
                        let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                        f(vm_ptr)
                    }
                    1 => {
                        let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                        f(vm_ptr, args_slice[0])
                    }
                    2 => {
                        let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                        f(vm_ptr, args_slice[0], args_slice[1])
                    }
                    3 => {
                        let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                        f(vm_ptr, args_slice[0], args_slice[1], args_slice[2])
                    }
                    _ => 0,
                };
            } else {
                return match n {
                    0 => {
                        let f: unsafe extern "C" fn() -> i64 = std::mem::transmute(entry);
                        f()
                    }
                    1 => {
                        let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                        f(args_slice[0])
                    }
                    2 => {
                        let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                        f(args_slice[0], args_slice[1])
                    }
                    3 => {
                        let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                        f(args_slice[0], args_slice[1], args_slice[2])
                    }
                    4 => {
                        let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                        f(args_slice[0], args_slice[1], args_slice[2], args_slice[3])
                    }
                    _ => 0,
                };
            }
        }
    }

    // Slow path: interpreter fallback
    let thread = match jit_thread_mut() {
        Some(t) => t,
        None => {
            return 0;
        }
    };

    let mut values = Vec::with_capacity(num_args.max(0) as usize);
    let mut desc_iter = DescriptorParamIter::new(info.descriptor);

    if info.invoke_kind != 3 {
        if !args_slice.is_empty() {
            let ptr = args_slice[0];
            if ptr == 0 {
                values.push(Value::Object(None));
            } else {
                // SAFETY: ptr is non-zero and was passed from JIT code as a receiver
                // object pointer, which points to a live heap object.
                values.push(Value::Object(Some(ObjectRef::from_raw(
                    ptr as usize as *mut u8,
                ))));
            }
        }
    }

    let start_idx = if info.invoke_kind != 3 { 1 } else { 0 };
    for &raw in &args_slice[start_idx..] {
        let val = match desc_iter.next() {
            Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => {
                Value::Int(raw as i32)
            }
            Some(b'J') => Value::Long(raw),
            Some(b'F') => Value::Float(f32::from_bits(raw as u32)),
            Some(b'D') => Value::Double(f64::from_bits(raw as u64)),
            Some(b'L') | Some(b'[') => {
                if raw == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ObjectRef::from_raw(raw as usize as *mut u8)))
                }
            }
            _ => Value::Int(raw as i32),
        };
        values.push(val);
    }

    let result: Option<Value> = match info.invoke_kind {
        0 | 2 => {
            if values.is_empty() {
                return 0;
            }
            let receiver_ref = match &values[0] {
                Value::Object(Some(obj)) => *obj,
                _ => return 0,
            };
            let method_args: Vec<Value> = values[1..].to_vec();
            let virt_result = {
                let mut ctx = crate::vm::NativeContextImpl { shared: vm, thread };
                use crate::native::registry::NativeContext;
                ctx.invoke_virtual(
                    receiver_ref,
                    info.method_name,
                    info.descriptor,
                    &method_args,
                )
            };
            match virt_result {
                Ok(v) => v,
                Err(e) => {
                    // S111r12 — JIT virtual-dispatch rescue: when the
                    // receiver's `class_id_of` returns a stub class
                    // (e.g. `java/lang/Comparable` for a malformed
                    // ClassLoader instance) that doesn't declare the
                    // CP-resolved method, `invoke_virtual` raises
                    // `NoSuchMethodError`. The CP method-ref class
                    // carried in `info.class_name` (e.g.
                    // `java/lang/ClassLoader`) is the spec-correct
                    // resolution target — retry the dispatch through
                    // it. Mirrors the S111r10 receiver-walk fallback
                    // for invokeinterface and the S111r8 cid=0 →
                    // CP-class fallback in `execute_invoke`.
                    let is_nsme = matches!(
                        &e,
                        crate::error::MethodCallFailed::InternalError(
                            crate::error::VmError::Linkage(
                                crate::error::LinkageError::NoSuchMethodError { .. },
                            ),
                        ),
                    );
                    if is_nsme && !info.class_name.is_empty() {
                        let recv_cid = vm.heap.class_id_of(receiver_ref);
                        let recv_name_opt = {
                            let cm = vm.class_manager.read();
                            cm.get_class(recv_cid)
                                .map(|c| c.name.to_string())
                        };
                        let cp_differs = recv_name_opt
                            .as_deref()
                            .map(|n| n != info.class_name)
                            .unwrap_or(true);
                        if cp_differs {
                            let r = crate::vm::invoke_or_native(
                                vm,
                                thread,
                                info.class_name,
                                info.method_name,
                                info.descriptor,
                                &values,
                            );
                            match r {
                                Ok(v) => v,
                                Err(e2) => {
                                    handle_jit_dispatch_error(vm, thread, e2, info);
                                    return 0;
                                }
                            }
                        } else {
                            handle_jit_dispatch_error(vm, thread, e, info);
                            return 0;
                        }
                    } else {
                        handle_jit_dispatch_error(vm, thread, e, info);
                        return 0;
                    }
                }
            }
        }
        1 | 3 => {
            let r = crate::vm::invoke_or_native(
                vm,
                thread,
                info.class_name,
                info.method_name,
                info.descriptor,
                &values,
            );
            match r {
                Ok(v) => v,
                Err(e) => {
                    handle_jit_dispatch_error(vm, thread, e, info);
                    return 0;
                }
            }
        }
        _ => None,
    };

    let ret = match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    };
    if std::env::var_os("RUSTJVM_DBG_JIT_DISPATCH").is_some() {
        eprintln!(
            "[JIT_DISPATCH_RET] {}.{}{} ret=0x{:x} ({})",
            info.class_name, info.method_name, info.descriptor, ret, ret,
        );
    }
    ret
}

/// Try to compile a callee method from a JitInvokeInfo.
/// Returns (entry_ptr, needs_context) if compilation succeeds.
// SAFETY: Caller must ensure vm is a valid SharedVm reference and info points to a live
// JitInvokeInfo. Delegates to try_jit_compile_callee which accesses the class manager
// and JIT compiler; no raw pointer dereferences occur within this function itself.
unsafe fn try_compile_callee(vm: &SharedVm, info: &JitInvokeInfo) -> Option<(usize, bool)> {
    use crate::runtime::interpreter::try_jit_compile_callee;
    try_jit_compile_callee(vm, info.class_name, info.method_name, info.descriptor)
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// info_ptr must point to a live JitInvokeInfo. args_ptr/num_args form a valid i64 slice.
// mic_ptr must point to a live JitMICSlot used for monomorphic inline cache dispatch.
// Transmutes within this function convert cached JIT entry pointers to function pointers
// matching the compiled method's extern "C" calling convention.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn jit_invoke_virtual_mic(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    mic_ptr: i64,
) -> i64 {
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    if num_args < 0 || (num_args > 0 && (args_ptr as *const i64).is_null()) {
        return 0;
    }
    let args_slice = if num_args == 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };

    let thread = match jit_thread_mut() {
        Some(t) => t,
        None => return 0,
    };

    let mut values = Vec::with_capacity(num_args.max(0) as usize);
    let mut desc_iter = DescriptorParamIter::new(info.descriptor);

    if args_slice.is_empty() {
        return 0;
    }
    let receiver_raw = args_slice[0];
    if receiver_raw == 0 {
        return 0;
    }
    let receiver_ref = ObjectRef::from_raw(receiver_raw as usize as *mut u8);
    values.push(Value::Object(Some(receiver_ref)));

    for &raw in &args_slice[1..] {
        let val = match desc_iter.next() {
            Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => {
                Value::Int(raw as i32)
            }
            Some(b'J') => Value::Long(raw),
            Some(b'F') => Value::Float(f32::from_bits(raw as u32)),
            Some(b'D') => Value::Double(f64::from_bits(raw as u64)),
            Some(b'L') | Some(b'[') => {
                if raw == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ObjectRef::from_raw(raw as usize as *mut u8)))
                }
            }
            _ => Value::Int(raw as i32),
        };
        values.push(val);
    }

    let receiver_class_id = vm.heap.class_id_of(receiver_ref);
    let receiver_cid = receiver_class_id.as_u32();

    let mic = &*(mic_ptr as *const JitMICSlot);
    let cached_cid = mic
        .cached_class_id
        .load(std::sync::atomic::Ordering::Acquire);

    if std::env::var_os("RUSTJVM_DBG_JIT_MIC").is_some() {
        eprintln!(
            "[JIT_MIC] {}.{}{} cached_cid={} recv_cid={} entry={}",
            info.class_name,
            info.method_name,
            info.descriptor,
            cached_cid,
            receiver_cid,
            mic.cached_entry_ptr.load(std::sync::atomic::Ordering::Acquire),
        );
    }

    // --- Monomorphic Inline Cache: fast path ---
    // If the receiver ClassId matches the cached value AND we have a cached
    // entry pointer, dispatch directly without any class_manager lookup or
    // method resolution.  This is the zero-overhead dispatch path.
    if cached_cid == receiver_cid && cached_cid != 0 {
        mic.record_hit();

        // Try the cached compiled entry pointer (true inline cache hit)
        let entry = mic.cached_entry_ptr.load(std::sync::atomic::Ordering::Acquire);
        if entry != 0 {
            // Direct call to cached compiled method — skip all resolution.
            // The cached entry is a function pointer with the same signature as
            // jit_invoke_dispatch: (vm_ptr, info_ptr, args_ptr, num_args) -> i64.
            // We can call it directly since it was resolved for this exact method.
            let cached_fn: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 =
                std::mem::transmute(entry as usize);
            return cached_fn(vm_ptr, info_ptr, args_ptr, num_args);
        }

        // Entry not cached yet — use cached class name for fast dispatch.
        let class_name: String = {
            let guard = mic.cached_class_name.lock();
            match &*guard {
                Some(name) => name.clone(),
                None => {
                    drop(guard);
                    let cm = vm.class_manager.read();
                    cm.get_class(receiver_class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default()
                }
            }
        };

        let method_args: Vec<Value> = values[1..].to_vec();
        let mut full_args = Vec::with_capacity(1 + method_args.len());
        full_args.push(Value::Object(Some(receiver_ref)));
        full_args.extend_from_slice(&method_args);

        // Try to compile callee for next time (populate cached_entry_ptr)
        if let Some((entry_ptr, _needs_ctx)) =
            try_compile_callee(vm, info)
        {
            mic.cached_entry_ptr
                .store(entry_ptr as u64, std::sync::atomic::Ordering::Release);
        }

        let invoke_res = crate::vm::invoke_or_native(
            vm,
            thread,
            &class_name,
            info.method_name,
            info.descriptor,
            &full_args,
        );
        // S111r12 — JIT MIC fast-path rescue: same CP-class fallback
        // as the cache-miss branch below (see comment there).
        let result = match invoke_res {
            Ok(v) => v,
            Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Linkage(
                    crate::error::LinkageError::NoSuchMethodError { .. },
                ),
            )) if !info.class_name.is_empty()
                && &*class_name != info.class_name =>
            {
                crate::vm::invoke_or_native(
                    vm,
                    thread,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    &full_args,
                )
                .unwrap_or_else(|e2| {
                    tracing::error!("JIT MIC fast-path CP-class rescue error: {:?}", e2);
                    None
                })
            }
            Err(e) => {
                tracing::error!("JIT virtual MIC dispatch error: {:?}", e);
                None
            }
        };

        return match result {
            Some(Value::Int(v)) => v as i64,
            Some(Value::Long(v)) => v,
            Some(Value::Float(f)) => f.to_bits() as i64,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
            Some(Value::Object(None)) | None => 0,
            _ => 0,
        };
    }

    // --- Cache miss: full resolution + update cache ---
    mic.record_miss();

    let class_name = {
        let cm = vm.class_manager.read();
        cm.get_class(receiver_class_id)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    };

    // Try to compile callee for cached entry
    let entry_ptr = try_compile_callee(vm, info)
        .map(|(ptr, _)| ptr as u64)
        .unwrap_or(0);

    // Update all MIC fields atomically
    mic.update(receiver_cid, &class_name, entry_ptr, false);

    let method_args: Vec<Value> = values[1..].to_vec();
    let mut full_args = Vec::with_capacity(1 + method_args.len());
    full_args.push(Value::Object(Some(receiver_ref)));
    full_args.extend_from_slice(&method_args);

    let invoke_res = crate::vm::invoke_or_native(
        vm,
        thread,
        &class_name,
        info.method_name,
        info.descriptor,
        &full_args,
    );
    // S111r12 — JIT MIC virtual-dispatch rescue. When the receiver's
    // runtime class (e.g. `java/lang/Comparable` for a malformed
    // ClassLoader instance) does not declare the CP-resolved method,
    // `invoke_or_native` raises `NoSuchMethodError`. The CP method-ref
    // class carried in `info.class_name` (e.g. `java/lang/ClassLoader`)
    // is the spec-correct resolution target — retry through it.
    // Mirrors the S111r10 receiver-walk fallback for invokeinterface
    // and the S111r8 cid=0 → CP-class fallback in `execute_invoke`.
    let result = match invoke_res {
        Ok(v) => v,
        Err(crate::error::MethodCallFailed::InternalError(
            crate::error::VmError::Linkage(
                crate::error::LinkageError::NoSuchMethodError { .. },
            ),
        )) if !info.class_name.is_empty()
            && &*class_name != info.class_name =>
        {
            crate::vm::invoke_or_native(
                vm,
                thread,
                info.class_name,
                info.method_name,
                info.descriptor,
                &full_args,
            )
            .unwrap_or_else(|e2| {
                tracing::error!("JIT MIC CP-class rescue error: {:?}", e2);
                None
            })
        }
        Err(e) => {
            tracing::error!("JIT virtual MIC dispatch error: {:?}", e);
            None
        }
    };

    match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Uncommon Trap / Deoptimization
// ---------------------------------------------------------------------------

/// Deopt reason codes passed from JIT-compiled code.
/// These map to `rustjvm_jit::deopt::DeoptReason` variants.
pub const DEOPT_REASON_NULL_CHECK: i64 = 0;
pub const DEOPT_REASON_CLASS_CHECK: i64 = 1;
pub const DEOPT_REASON_BOUNDS_CHECK: i64 = 2;
pub const DEOPT_REASON_DIV_BY_ZERO: i64 = 3;
pub const DEOPT_REASON_RECEIVER_TYPE_CHANGED: i64 = 4;
pub const DEOPT_REASON_CLASS_LOADING: i64 = 5;
pub const DEOPT_REASON_UNCOMMON_TRAP: i64 = 6;
pub const DEOPT_REASON_SPECULATION_FAILED: i64 = 7;
pub const DEOPT_REASON_UNREACHED_CODE: i64 = 8;

/// Deopt action codes returned from `jit_uncommon_trap`.
pub const DEOPT_ACTION_REINTERPRET: i64 = 0;
pub const DEOPT_ACTION_RECOMPILE: i64 = 1;
pub const DEOPT_ACTION_BLACKLIST: i64 = 2;

pub fn reason_code_to_deopt_reason(code: i64) -> rustjvm_jit::deopt::DeoptReason {
    match code {
        DEOPT_REASON_NULL_CHECK => rustjvm_jit::deopt::DeoptReason::NullCheck,
        DEOPT_REASON_CLASS_CHECK => rustjvm_jit::deopt::DeoptReason::ClassCheck,
        DEOPT_REASON_BOUNDS_CHECK => rustjvm_jit::deopt::DeoptReason::BoundsCheck,
        DEOPT_REASON_DIV_BY_ZERO => rustjvm_jit::deopt::DeoptReason::DivByZero,
        DEOPT_REASON_RECEIVER_TYPE_CHANGED => rustjvm_jit::deopt::DeoptReason::ReceiverTypeChanged,
        DEOPT_REASON_CLASS_LOADING => rustjvm_jit::deopt::DeoptReason::ClassLoading,
        DEOPT_REASON_UNCOMMON_TRAP => rustjvm_jit::deopt::DeoptReason::UncommonTrap,
        DEOPT_REASON_SPECULATION_FAILED => rustjvm_jit::deopt::DeoptReason::SpeculationFailed,
        DEOPT_REASON_UNREACHED_CODE => rustjvm_jit::deopt::DeoptReason::UnreachedCode,
        _ => rustjvm_jit::deopt::DeoptReason::UncommonTrap,
    }
}

/// Orchestrates deoptimization: records the event, invalidates compiled code,
/// and queues recompilation based on the deopt log's recommended action.
pub struct DeoptimizationController;

impl DeoptimizationController {
    /// Execute a full deoptimization cycle for a method.
    ///
    /// 1. Record the deopt event in the deopt log
    /// 2. Invalidate the compiled method in the JIT cache
    /// 3. Notify the tiered compilation manager
    /// 4. If receiver type changed, invalidate via the invalidation manager
    /// 5. Return the recommended action
    pub fn deoptimize(
        vm: &SharedVm,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        reason: rustjvm_jit::deopt::DeoptReason,
        bci: u32,
    ) -> rustjvm_jit::deopt::DeoptAction {
        // Build method key for deopt log
        let method_key = format!("{}.{}:{}", class_name, method_name, descriptor);

        // Create the deopt event
        let event = rustjvm_jit::deopt::DeoptEvent {
            reason,
            action: rustjvm_jit::deopt::DeoptAction::Reinterpret, // initial; may be overridden
            bci,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            speculation_id: 0,
        };

        // Record in deopt log and get recommended action
        let tiered_key = rustjvm_jit::tiered::MethodKey {
            class_name: class_name.to_string(),
            method_name: method_name.to_string(),
            descriptor: descriptor.to_string(),
        };
        let action = vm.record_deoptimization(&method_key, event, &tiered_key);

        // Invalidate the compiled method from the JIT cache
        {
            let mut jit_cache = vm.jit_cache.write();
            jit_cache.remove(class_name, method_name, descriptor);
        }

        // For class-check or receiver-type failures, also check the
        // invalidation manager for dependent methods.
        if matches!(
            reason,
            rustjvm_jit::deopt::DeoptReason::ReceiverTypeChanged
                | rustjvm_jit::deopt::DeoptReason::ClassCheck
                | rustjvm_jit::deopt::DeoptReason::ClassLoading
        ) {
            let mut inv_mgr = vm.invalidation_manager.lock();
            // Clear stale assumptions for the deoptimized method
            inv_mgr.clear_assumptions(&method_key);
        }

        // If the deopt log recommends giving up, add to the JIT skip set
        if action == rustjvm_jit::deopt::DeoptAction::MakeNotCompilable {
            let mut skip = vm.jit_skip_set.write();
            skip.insert((
                class_name.into(),
                method_name.into(),
                descriptor.into(),
            ));
        }

        tracing::debug!(
            "deopt: {} reason={:?} bci={} action={:?}",
            method_key, reason, bci, action
        );

        // Emit JFR deoptimization event
        {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let mut jfr = vm.flight_recorder.lock();
            rustjvm_jfr::builtin::emit_deoptimization_event(
                &mut jfr,
                &method_key,
                0, // compile_id
                &format!("{:?}", reason),
                &format!("{:?}", action),
                bci as i32,
                0, // thread_id
                now_ns,
            );
        }

        action
    }
}

/// JIT runtime helper: called from compiled code when a speculative
/// optimization fails (uncommon trap).
///
/// Signature: extern "C" fn(vm_ptr: i64, reason: i64, bci: i64) -> i64
///
/// The reason parameter encodes a `DeoptReason` variant as an integer.
/// Returns a deopt action code:
///   0 = reinterpret (continue in interpreter)
///   1 = recompile (invalidate and recompile with updated profile)
///   2 = blacklist (never compile again)
///
/// After this returns, the JIT code should return control to the interpreter.
/// The calling convention is that the JIT method returns a sentinel value
/// (i64::MIN) to signal "deoptimized, resume in interpreter".
// SAFETY: Called from JIT-compiled code when a speculative optimization fails.
// vm_ptr must be 0 or a valid SharedVm pointer. reason encodes a DeoptReason variant.
// bci is the bytecode index of the failing instruction. Accesses the JIT thread pointer
// (via jit_thread_mut) and the deoptimization controller to invalidate compiled code.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn jit_uncommon_trap(
    vm_ptr: i64,
    reason: i64,
    bci: i64,
) -> i64 {
    if vm_ptr == 0 {
        return DEOPT_ACTION_REINTERPRET;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let deopt_reason = reason_code_to_deopt_reason(reason);

    // Try to determine the method being executed from the JIT thread context.
    // If we can't determine the method, we still record the deopt but with a
    // generic key.
    let (class_name, method_name, descriptor) = {
        // The thread's current frame has the method info
        let default = ("unknown".to_string(), "unknown".to_string(), "()V".to_string());
        if let Some(thread) = jit_thread_mut() {
            if let Some(frame) = thread.frames.last() {
                (
                    frame.class_name().to_string(),
                    frame.method_name().to_string(),
                    frame.method_descriptor().to_string(),
                )
            } else {
                default
            }
        } else {
            default
        }
    };

    let action = DeoptimizationController::deoptimize(
        vm,
        &class_name,
        &method_name,
        &descriptor,
        deopt_reason,
        bci as u32,
    );

    match action {
        rustjvm_jit::deopt::DeoptAction::Reinterpret => DEOPT_ACTION_REINTERPRET,
        rustjvm_jit::deopt::DeoptAction::RecompileAndReinterpret => DEOPT_ACTION_RECOMPILE,
        rustjvm_jit::deopt::DeoptAction::MakeNotEntrant => DEOPT_ACTION_RECOMPILE,
        rustjvm_jit::deopt::DeoptAction::MakeNotCompilable => DEOPT_ACTION_BLACKLIST,
    }
}

// ---------------------------------------------------------------------------
// Helper table construction
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_checkcast_null_ptr_returns_zero() {
        // SAFETY: Passing all-zero/null arguments exercises the null-object fast path;
        // no heap pointers are dereferenced.
        let result = unsafe { jit_checkcast(0, 0, std::ptr::null(), 0) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_instanceof_null_ptr_returns_zero() {
        // SAFETY: Passing all-zero/null arguments exercises the null-object fast path;
        // no heap pointers are dereferenced.
        let result = unsafe { jit_instanceof(0, 0, std::ptr::null(), 0) };
        assert_eq!(result, 0);
    }

    /// NEW-1.2 regression: a typecheck site with a missing class-name pointer
    /// is a JIT-side bug (typecheck_info should always be populated by the
    /// scanner). The safe behavior is to *fail closed* — return 0 from
    /// checkcast, which the JIT-compiled code interprets as a failed cast and
    /// surfaces as a deterministic ClassCastException through the interpreter
    /// fallback. The old behavior silently let the cast through, masking the
    /// underlying scan/compile mismatch.
    #[test]
    fn jit_checkcast_negative_len_fails_closed() {
        // SAFETY: obj_ptr is 0 (null) and class_name_len is negative, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_checkcast(0, 42, "test".as_ptr(), -1) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_instanceof_negative_len_returns_zero() {
        // SAFETY: obj_ptr is 0 (null) and class_name_len is negative, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_instanceof(0, 42, "test".as_ptr(), -1) };
        assert_eq!(result, 0);
    }

    /// NEW-1.2 regression: same fail-closed semantics for a null class-name
    /// pointer with a positive length.
    #[test]
    fn jit_checkcast_null_class_name_ptr_fails_closed() {
        // SAFETY: obj_ptr is 0 (null) and class_name_ptr is null, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_checkcast(0, 99, std::ptr::null(), 5) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_instanceof_null_class_name_ptr_returns_zero() {
        // SAFETY: obj_ptr is 0 (null) and class_name_ptr is null, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_instanceof(0, 99, std::ptr::null(), 5) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_baload_null_returns_zero() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        let result = unsafe { jit_baload(0, 0) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_iaload_null_returns_zero() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        let result = unsafe { jit_iaload(0, 0) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_aaload_null_returns_zero() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        let result = unsafe { jit_aaload(0, 0) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_arraylength_null_returns_neg_one() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        let result = unsafe { jit_arraylength(0) };
        assert_eq!(result, -1);
    }

    #[test]
    fn jit_getfield_null_returns_zero() {
        // SAFETY: obj_ptr is 0 (null), so the function returns early without dereferencing.
        let result = unsafe { jit_getfield(0, 0) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_thread_cleared_returns_none() {
        clear_jit_thread();
        // SAFETY: The JIT thread pointer was just cleared above, so jit_thread_mut
        // returns None without dereferencing any pointer.
        let result = unsafe { jit_thread_mut() };
        assert!(result.is_none());
    }
}

/// Build the JIT runtime helpers table with real function pointer addresses.
pub fn build_helpers() -> JitRuntimeHelpers {
    JitRuntimeHelpers {
        newarray: jit_newarray as *const () as usize,
        new_object: jit_new_object as *const () as usize,
        anewarray_object: jit_anewarray_object as *const () as usize,
        baload: jit_baload as *const () as usize,
        bastore: jit_bastore as *const () as usize,
        iaload: jit_iaload as *const () as usize,
        iastore: jit_iastore as *const () as usize,
        aaload: jit_aaload as *const () as usize,
        aastore: jit_aastore as *const () as usize,
        multianewarray_2d: jit_multianewarray_2d as *const () as usize,
        arraylength: jit_arraylength as *const () as usize,
        getfield: jit_getfield as *const () as usize,
        putfield_int: jit_putfield_int as *const () as usize,
        putfield_long: jit_putfield_long as *const () as usize,
        putfield_float: jit_putfield_float as *const () as usize,
        putfield_double: jit_putfield_double as *const () as usize,
        putfield_object: jit_putfield_object as *const () as usize,
        getstatic: jit_getstatic as *const () as usize,
        putstatic_int: jit_putstatic_int as *const () as usize,
        putstatic_long: jit_putstatic_long as *const () as usize,
        putstatic_float: jit_putstatic_float as *const () as usize,
        putstatic_double: jit_putstatic_double as *const () as usize,
        putstatic_object: jit_putstatic_object as *const () as usize,
        checkcast: jit_checkcast as *const () as usize,
        instanceof_check: jit_instanceof as *const () as usize,
        throw_aioobe: jit_throw_aioobe as *const () as usize,
        invoke_dispatch: jit_invoke_dispatch as *const () as usize,
        invoke_virtual_mic: jit_invoke_virtual_mic as *const () as usize,
        write_barrier: jit_write_barrier as *const () as usize,
        uncommon_trap: jit_uncommon_trap as *const () as usize,
        math_fma_double: jit_math_fma_double as *const () as usize,
        math_fma_float: jit_math_fma_float as *const () as usize,
    }
}

/// T1.1.28 — Math.fma(double, double, double) runtime helper.
///
/// Called from JIT code via an absolute CALL emitted by the
/// `MATH_FMA_DOUBLE_INTRINSIC` path in `jit/src/x64.rs`. Delegates to
/// Rust's `f64::mul_add`, which compiles to `VFMADD231SD` on x86-64
/// hosts with FMA3 and to a correctly-rounded software implementation
/// otherwise. Both paths satisfy the JLS `Math.fma` contract of
/// "compute `a*b + c` as if with unlimited intermediate precision,
/// then round once".
#[no_mangle]
pub extern "C" fn jit_math_fma_double(a: f64, b: f64, c: f64) -> f64 {
    a.mul_add(b, c)
}

/// T1.1.28 — Math.fma(float, float, float) runtime helper.
#[no_mangle]
pub extern "C" fn jit_math_fma_float(a: f32, b: f32, c: f32) -> f32 {
    a.mul_add(b, c)
}
