// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Part C — primitive-array marshalling between the JVM heap and CUDA device
//! memory.
//!
//! The whole module is gated behind the `gpu-offload` Cargo feature on
//! `cratonvm-vm`. With the feature off, this file is not compiled and no
//! GPU-related symbols leak into the default build.
//!
//! ## What lives here
//!
//! - `host_view_<T>(obj, heap)` — copy a JVM primitive array into a packed
//!   host `Vec<T>` suitable for upload to a CUDA device.
//! - `write_back_<T>(obj, heap, src)` — copy a packed host slice back into
//!   a JVM primitive array's storage.
//! - `upload` / `download_into` — thin wrappers over `cuda_bridge::DeviceBuffer`
//!   so callers don't have to import `cuda-bridge` directly.
//!
//! ## Layout note
//!
//! The original plan in
//! [docs/plans](../../../../) speculated that primitive arrays on the JVM
//! heap used a 16-byte slot stride (`SLOT_SIZE`). In the current
//! `cratonvm-gc` layout, primitive arrays in fact use **native element
//! sizes** (`element_byte_size` in `cratonvm_types::heap_types`): 4 bytes
//! for `int`/`float`, 8 bytes for `long`/`double`, 2 for `short`/`char`,
//! 1 for `byte`/`boolean`. We therefore go through the heap's existing
//! `get_array_element` / `set_array_element` accessors — they already
//! know the correct stride and bounds — and avoid reinventing pointer
//! arithmetic in this module. That keeps the marshalling code resilient
//! to future heap-layout changes (e.g. a compact-array migration) without
//! re-touching this file.
//!
//! ## Safepoints (Part F integration — wired in Part E)
//!
//! Every public marshal function takes a `_token: &SafepointToken<'_>`
//! parameter (marker-only — note the leading underscore in the function
//! signatures). The token is purely a type-system marker: the fact that
//! we hold one proves the heap is in a no-GC critical section. Callers
//! acquire the token via `Heap::enter_gpu_critical` (or the VM's
//! `SharedVm::enter_gpu_critical` wrapper) before touching the heap
//! through these functions; the GC delays collection while the token is
//! alive.

use cratonvm_gc::safepoint::SafepointToken;
use cratonvm_gc::VmHeap;
use cratonvm_types::{ArrayElementType, ObjectKind, ObjectRef, Value};

pub use cuda_bridge::{
    DeviceBuffer, DeviceContext, DeviceElem, DeviceError, Result as DeviceResult,
};

// ── Host views (heap → packed Vec<T>) ────────────────────────────────────

/// Copy a Java `int[]` out of the JVM heap into a packed host `Vec<i32>`.
///
/// # Panics
///
/// Panics if `obj` is not a heap-allocated `int[]`. This is an internal
/// invariant — Part D's analyzer + Part E's caller verify the array shape
/// before reaching this point.
pub fn host_view_i32(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<i32> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_i32: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Int,
        "host_view_i32: not an int[]"
    );
    let len = header.array_length() as usize;
    // PERF: `int[]` elements are stored as a contiguous native-`i32`
    // little-endian region starting at `array_data_ptr`. A single
    // `copy_nonoverlapping` replaces `len` boxed `Value`-returning
    // accessor calls (each with a bounds check + enum match) — for a
    // 2^24-element array that is 16.7M calls eliminated per array per
    // marshal. The SafepointToken proves the heap is in a no-GC
    // critical section, so the payload cannot move under the copy.
    let mut out = vec![0i32; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: `obj` is a live `int[]` (kind + element_type asserted
                // above); its payload is `len * 4` contiguous bytes at
                // `array_data_ptr`. `out` was just allocated with `len` i32s.
                // Source (heap arena) and destination (fresh Vec) do not
                // overlap. The GC is paused (token held), so `src` stays valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 4);
                }
            }
            // G1 humongous `int[]`: payload spans non-contiguous regions, so
            // there is no flat base pointer. Fall back to the region-safe
            // per-element accessor (see `host_view_i16`).
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_i32: in-bounds index returned OOB");
                    match v {
                        Value::Int(x) => *slot = x,
                        other => panic!("host_view_i32: expected Value::Int, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

/// Copy a Java `long[]` into a packed host `Vec<i64>`.
pub fn host_view_i64(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<i64> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_i64: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Long,
        "host_view_i64: not a long[]"
    );
    let len = header.array_length() as usize;
    // PERF: bulk copy — see `host_view_i32`. `long[]` is contiguous
    // native `i64` little-endian.
    let mut out = vec![0i64; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: live `long[]` (asserted); `len * 8` contiguous bytes
                // at `array_data_ptr`; `out` holds `len` i64s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 8);
                }
            }
            // G1 humongous `long[]`: region-safe per-element fallback.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_i64: in-bounds index returned OOB");
                    match v {
                        Value::Long(x) => *slot = x,
                        other => panic!("host_view_i64: expected Value::Long, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

/// Copy a Java `float[]` into a packed host `Vec<f32>`.
pub fn host_view_f32(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<f32> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_f32: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Float,
        "host_view_f32: not a float[]"
    );
    let len = header.array_length() as usize;
    // PERF: bulk copy — see `host_view_i32`. `float[]` is contiguous
    // native `f32` little-endian (IEEE-754 bit pattern preserved).
    let mut out = vec![0f32; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: live `float[]` (asserted); `len * 4` contiguous bytes
                // at `array_data_ptr`; `out` holds `len` f32s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 4);
                }
            }
            // G1 humongous `float[]`: region-safe per-element fallback.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_f32: in-bounds index returned OOB");
                    match v {
                        Value::Float(x) => *slot = x,
                        other => panic!("host_view_f32: expected Value::Float, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

/// Copy a Java `double[]` into a packed host `Vec<f64>`.
pub fn host_view_f64(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<f64> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_f64: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Double,
        "host_view_f64: not a double[]"
    );
    let len = header.array_length() as usize;
    // PERF: bulk copy — see `host_view_i32`. `double[]` is contiguous
    // native `f64` little-endian (IEEE-754 bit pattern preserved).
    let mut out = vec![0f64; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: live `double[]` (asserted); `len * 8` contiguous bytes
                // at `array_data_ptr`; `out` holds `len` f64s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 8);
                }
            }
            // G1 humongous `double[]`: region-safe per-element fallback.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_f64: in-bounds index returned OOB");
                    match v {
                        Value::Double(x) => *slot = x,
                        other => panic!("host_view_f64: expected Value::Double, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

/// Copy a Java `short[]` into a packed host `Vec<i16>`.
pub fn host_view_i16(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<i16> {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "host_view_i16: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Short,
        "host_view_i16: not a short[]"
    );
    let len = header.array_length() as usize;
    // PERF: bulk copy — see `host_view_i32`. Unlike the value-based
    // `get_array_element` accessor (which boxes every element through
    // `Value::Int`, sign-extended), the *heap slot itself* is a native
    // `i16` at its natural 2-byte width — NOT widened to a 4-byte slot.
    // See the module doc's "Layout note" and
    // `cratonvm_types::heap_types::element_byte_size(ArrayElementType::Short)
    // == 2`, plus `read_prim_element`/`write_prim_element` in
    // `cratonvm-gc`, which read/write `i16` at `base + index * 2` via
    // `ptr::{read,write}_unaligned`. That packed layout is exactly what a
    // `copy_nonoverlapping` requires, so the same bulk path used for the
    // 32/64-bit types is legal here too.
    let mut out = vec![0i16; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: `obj` is a live `short[]` (kind + element_type
                // asserted above); its payload is `len * 2` contiguous bytes
                // at `array_data_ptr`. `out` was just allocated with `len`
                // i16s. Source (heap arena) and destination (fresh Vec) do
                // not overlap. The GC is paused (token held), so `src` stays
                // valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 2);
                }
            }
            // G1 humongous `short[]`: payload spans non-contiguous regions,
            // so there is no flat base pointer. Fall back to the
            // region-safe per-element accessor. Heap storage sign-extends
            // to `Value::Int`; we truncate back to `i16` on the way out.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_i16: in-bounds index returned OOB");
                    match v {
                        Value::Int(x) => *slot = x as i16,
                        other => panic!("host_view_i16: expected Value::Int, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

/// Copy a Java `byte[]` into a packed host `Vec<i8>`.
pub fn host_view_i8(obj: ObjectRef, heap: &VmHeap, _token: &SafepointToken<'_>) -> Vec<i8> {
    let header = heap.get_header(obj);
    assert_eq!(header.kind(), ObjectKind::Array, "host_view_i8: not an array");
    assert_eq!(
        header.element_type(),
        ArrayElementType::Byte,
        "host_view_i8: not a byte[]"
    );
    let len = header.array_length() as usize;
    // PERF: bulk copy — see `host_view_i32` / `host_view_i16`. `byte[]`
    // elements are stored one raw byte per slot at `base + index` (stride
    // 1, no widening — `element_byte_size(ArrayElementType::Byte) == 1`),
    // so a single `copy_nonoverlapping` of `len` bytes replaces `len`
    // boxed `Value::Int`-returning accessor calls.
    let mut out = vec![0i8; len];
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(src) => {
                // SAFETY: `obj` is a live `byte[]` (kind + element_type
                // asserted above); its payload is `len` contiguous bytes at
                // `array_data_ptr` (1-byte stride). `out` was just allocated
                // with `len` i8s. Source (heap arena) and destination
                // (fresh Vec) do not overlap. The GC is paused (token
                // held), so `src` stays valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len);
                }
            }
            // G1 humongous `byte[]`: region-safe per-element fallback (see
            // `host_view_i16`). Heap storage sign-extends to `Value::Int`;
            // we truncate to `i8` on the way out.
            None => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let v = heap
                        .get_array_element(obj, i)
                        .expect("host_view_i8: in-bounds index returned OOB");
                    match v {
                        Value::Int(x) => *slot = x as i8,
                        other => panic!("host_view_i8: expected Value::Int, got {other:?}"),
                    }
                }
            }
        }
    }
    out
}

// ── Write-back (packed slice → heap) ─────────────────────────────────────

/// Copy a packed host `&[i32]` into a JVM `int[]` on the heap.
///
/// # Panics
///
/// - if `obj` is not an `int[]` on the heap;
/// - if `src.len()` differs from the array's length. Callers (Part E) are
///   expected to enforce length equality before calling.
pub fn write_back_i32(obj: ObjectRef, heap: &VmHeap, src: &[i32], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_i32: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Int,
        "write_back_i32: not an int[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i32: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — `int[]` payload is contiguous native `i32`
    // little-endian, so one `copy_nonoverlapping` replaces `len`
    // boxed `set_array_element` calls. No write barrier is needed:
    // primitive-array stores never create cross-generation references.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: `obj` is a live `int[]` (asserted); payload is
                // `len * 4` contiguous bytes at `array_data_ptr`. `src` holds
                // exactly `len` i32s. Heap arena and `src` slice do not
                // overlap. GC is paused (token held), so `dst` stays valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 4);
                }
            }
            // G1 humongous `int[]`: region-safe per-element store.
            None => {
                for (i, &x) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Int(x))
                        .expect("write_back_i32: in-bounds index returned OOB");
                }
            }
        }
    }
}

/// Copy a packed host `&[i64]` into a JVM `long[]`.
pub fn write_back_i64(obj: ObjectRef, heap: &VmHeap, src: &[i64], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_i64: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Long,
        "write_back_i64: not a long[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i64: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — see `write_back_i32`.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: live `long[]` (asserted); `len * 8` contiguous bytes
                // at `array_data_ptr`; `src` holds `len` i64s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 8);
                }
            }
            // G1 humongous `long[]`: region-safe per-element store.
            None => {
                for (i, &x) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Long(x))
                        .expect("write_back_i64: in-bounds index returned OOB");
                }
            }
        }
    }
}

/// Copy a packed host `&[f32]` into a JVM `float[]`.
pub fn write_back_f32(obj: ObjectRef, heap: &VmHeap, src: &[f32], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_f32: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Float,
        "write_back_f32: not a float[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_f32: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — see `write_back_i32`.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: live `float[]` (asserted); `len * 4` contiguous bytes
                // at `array_data_ptr`; `src` holds `len` f32s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 4);
                }
            }
            // G1 humongous `float[]`: region-safe per-element store.
            None => {
                for (i, &x) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Float(x))
                        .expect("write_back_f32: in-bounds index returned OOB");
                }
            }
        }
    }
}

/// Copy a packed host `&[f64]` into a JVM `double[]`.
pub fn write_back_f64(obj: ObjectRef, heap: &VmHeap, src: &[f64], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_f64: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Double,
        "write_back_f64: not a double[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_f64: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — see `write_back_i32`.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: live `double[]` (asserted); `len * 8` contiguous bytes
                // at `array_data_ptr`; `src` holds `len` f64s; no overlap; GC
                // paused (token held).
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 8);
                }
            }
            // G1 humongous `double[]`: region-safe per-element store.
            None => {
                for (i, &x) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Double(x))
                        .expect("write_back_f64: in-bounds index returned OOB");
                }
            }
        }
    }
}

/// Copy a packed host `&[i16]` into a JVM `short[]` on the heap.
///
/// # Panics
///
/// - if `obj` is not a `short[]` on the heap;
/// - if `src.len()` differs from the array's length. Callers (Part E) are
///   expected to enforce length equality before calling.
pub fn write_back_i16(obj: ObjectRef, heap: &VmHeap, src: &[i16], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_i16: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Short,
        "write_back_i16: not a short[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i16: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — see `write_back_i32`. `short[]` slots are 2 bytes
    // at their natural width (see `host_view_i16`), so one
    // `copy_nonoverlapping` replaces `len` boxed `set_array_element`
    // calls. No write barrier is needed: primitive-array stores never
    // create cross-generation references.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: `obj` is a live `short[]` (asserted); payload is
                // `len * 2` contiguous bytes at `array_data_ptr`. `src`
                // holds exactly `len` i16s. Heap arena and `src` slice do
                // not overlap. GC is paused (token held), so `dst` stays
                // valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len * 2);
                }
            }
            // G1 humongous `short[]`: region-safe per-element store.
            // `set_array_element` truncates `Value::Int` to `i16` on write.
            None => {
                for (i, &v) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Int(v as i32))
                        .expect("write_back_i16: in-bounds index returned OOB");
                }
            }
        }
    }
}

/// Copy a packed host `&[i8]` into a JVM `byte[]` on the heap.
///
/// # Panics
///
/// - if `obj` is not a `byte[]` on the heap;
/// - if `src.len()` differs from the array's length. Callers (Part E) are
///   expected to enforce length equality before calling.
pub fn write_back_i8(obj: ObjectRef, heap: &VmHeap, src: &[i8], _token: &SafepointToken<'_>) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind(),
        ObjectKind::Array,
        "write_back_i8: not an array"
    );
    assert_eq!(
        header.element_type(),
        ArrayElementType::Byte,
        "write_back_i8: not a byte[]"
    );
    let len = header.array_length() as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i8: src length {} != array length {}",
        src.len(),
        len
    );
    // PERF: bulk store — see `write_back_i32` / `write_back_i16`. `byte[]`
    // slots are 1 byte (stride 1, no widening), so one
    // `copy_nonoverlapping` of `len` bytes replaces `len` boxed
    // `set_array_element` calls.
    if len != 0 {
        match heap.array_data_ptr(obj) {
            Some(dst) => {
                // SAFETY: `obj` is a live `byte[]` (asserted); payload is
                // `len` contiguous bytes at `array_data_ptr` (1-byte
                // stride). `src` holds exactly `len` i8s. Heap arena and
                // `src` slice do not overlap. GC is paused (token held),
                // so `dst` stays valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, len);
                }
            }
            // G1 humongous `byte[]`: region-safe per-element store (see
            // `write_back_i16`). `set_array_element` truncates
            // `Value::Int` to `u8` on write.
            None => {
                for (i, &v) in src.iter().enumerate() {
                    heap.set_array_element(obj, i, Value::Int(v as i32))
                        .expect("write_back_i8: in-bounds index returned OOB");
                }
            }
        }
    }
}

// ── Device buffer transfer wrappers ──────────────────────────────────────

/// Allocate a device buffer and upload `host` into it in one shot.
///
/// On a machine without a CUDA driver (or when `cuda-bridge` was built
/// without the `cuda` feature), this returns `Err(DeviceError::NoDriver)`.
pub fn upload<T>(ctx: &DeviceContext, host: &[T]) -> DeviceResult<DeviceBuffer<T>>
where
    T: DeviceElem,
{
    DeviceBuffer::from_host(ctx, host)
}

/// Copy `buf`'s contents back into `dst`. `dst.len()` must be at least
/// `buf.len()`; `cuda-bridge` enforces this at the driver layer.
pub fn download_into<T>(buf: &DeviceBuffer<T>, dst: &mut [T]) -> DeviceResult<()>
where
    T: DeviceElem,
{
    buf.to_host(dst)
}

// ── Direct heap↔device transfer (skips the intermediate host Vec) ─────────
//
// `host_view_<T>` + `upload` stage through a packed host `Vec` (one memcpy on
// the way in, another on the way out via `download_into` + `write_back_<T>`).
// For an *ordinary* (contiguous) primitive array we can skip that staging
// buffer and let CUDA's H2D/D2H DMA read/write the JVM heap arena in place:
// the `SafepointToken` proves the GC is paused, so the arena pointer is stable
// and no Java thread touches the array for the duration of the copy. This
// removes a full array-sized host memcpy per direction (≈1 GiB at 2^28),
// which is the residual warm-path gap vs TornadoVM's native off-heap arrays.
// G1-humongous arrays (no flat base pointer) fall back to the staged path.

/// `false` only when `CRATONVM_GPU_NO_ZEROCOPY` is set — an opt-out for A/B
/// measurement / safety. Cached.
fn zerocopy_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_GPU_NO_ZEROCOPY").is_none())
}

/// Returns `true` only when `obj`'s header describes a primitive array whose
/// element type is exactly `expected`. This is the gate the zero-copy
/// direct-transfer path must clear before reinterpreting the heap arena as a
/// `&[$ty]`: `from_raw_parts(src as *const $ty, len)` reads
/// `len * size_of::<$ty>()` bytes, so a mismatched element width (e.g. an
/// `int[]` viewed as `long[]`) — or a non-array object whose `array_length`
/// field aliases unrelated bytes — would read or write off the end of the
/// real payload, corrupting the heap. The staged fallback validates kind +
/// element type inside `host_view_*` / `write_back_*`; the zero-copy branch
/// skips those helpers, so it must perform the same check itself.
fn zerocopy_shape_ok(obj: ObjectRef, heap: &VmHeap, expected: ArrayElementType) -> bool {
    let header = heap.get_header(obj);
    header.kind() == ObjectKind::Array && header.element_type() == expected
}

macro_rules! direct_xfer {
    ($up:ident, $down:ident, $ty:ty, $elem:expr, $host_view:path, $write_back:path, $what:literal) => {
        #[doc = concat!("Upload a Java `", $what, "` to a fresh device buffer, reading the heap")]
        #[doc = "arena directly when the array is contiguous (else staged via a host `Vec`)."]
        pub fn $up(
            ctx: &DeviceContext,
            obj: ObjectRef,
            heap: &VmHeap,
            token: &SafepointToken<'_>,
        ) -> DeviceResult<DeviceBuffer<$ty>> {
            let len = heap.get_header(obj).array_length() as usize;
            // Only take the zero-copy fast path once we have proven the object
            // really is an array of the expected element type — otherwise the
            // raw `from_raw_parts` below would read past the payload (OOB).
            // A failed check (wrong kind / element type) falls through to the
            // staged path, whose `host_view_*` asserts produce a clean panic.
            if zerocopy_enabled() && len != 0 && zerocopy_shape_ok(obj, heap, $elem) {
                if let Some(src) = heap.array_data_ptr(obj) {
                    // SAFETY: `obj` is a live contiguous `$elem` array of `len`
                    // elements at `src` (native-aligned primitive storage, kind
                    // + element type verified by `zerocopy_shape_ok`, so the
                    // payload is exactly `len * size_of::<$ty>()` bytes); the
                    // held `token` pins the GC, so the arena cannot move and no
                    // Java thread observes it during this read-only upload.
                    let slice = unsafe { std::slice::from_raw_parts(src as *const $ty, len) };
                    return DeviceBuffer::from_host(ctx, slice);
                }
            }
            DeviceBuffer::from_host(ctx, &$host_view(obj, heap, token))
        }

        #[doc = concat!("Download a device buffer back into a Java `", $what, "`, writing the heap")]
        #[doc = "arena directly when the array is contiguous (else staged + write-back)."]
        pub fn $down(
            buf: &DeviceBuffer<$ty>,
            obj: ObjectRef,
            heap: &VmHeap,
            token: &SafepointToken<'_>,
        ) -> DeviceResult<()> {
            let len = heap.get_header(obj).array_length() as usize;
            // As in `$up`: validate kind + element type before reinterpreting
            // the arena as `&mut [$ty]`, or a mismatched element width would
            // write `len * size_of::<$ty>()` bytes off the end of the real
            // payload, corrupting the heap. A failed check falls through to
            // the staged path (`write_back_*` re-asserts the shape).
            if zerocopy_enabled() && len != 0 && zerocopy_shape_ok(obj, heap, $elem) {
                if let Some(dst) = heap.array_data_ptr(obj) {
                    // SAFETY: live contiguous `$elem` array of `len` elements at
                    // `dst` (kind + element type verified by `zerocopy_shape_ok`,
                    // so the payload is exactly `len * size_of::<$ty>()` bytes);
                    // GC paused (token) so this dispatch thread has exclusive
                    // access to the arena for the device→host copy.
                    let slice = unsafe { std::slice::from_raw_parts_mut(dst as *mut $ty, len) };
                    return buf.to_host(slice);
                }
            }
            let mut staged = vec![<$ty>::default(); len];
            buf.to_host(&mut staged)?;
            $write_back(obj, heap, &staged, token);
            Ok(())
        }
    };
}

direct_xfer!(
    upload_obj_i32,
    download_obj_i32,
    i32,
    ArrayElementType::Int,
    host_view_i32,
    write_back_i32,
    "int[]"
);
direct_xfer!(
    upload_obj_i64,
    download_obj_i64,
    i64,
    ArrayElementType::Long,
    host_view_i64,
    write_back_i64,
    "long[]"
);
direct_xfer!(
    upload_obj_f32,
    download_obj_f32,
    f32,
    ArrayElementType::Float,
    host_view_f32,
    write_back_f32,
    "float[]"
);
direct_xfer!(
    upload_obj_f64,
    download_obj_f64,
    f64,
    ArrayElementType::Double,
    host_view_f64,
    write_back_f64,
    "double[]"
);
// `short[]`/`byte[]` admit the same zero-copy `from_raw_parts` reinterpret
// as the 32/64-bit types above: their heap slots are natural-width and
// packed (see `host_view_i16`/`host_view_i8`), so `zerocopy_shape_ok` +
// `array_data_ptr` give exactly the same contiguous-span guarantee. The
// element types also satisfy `cuda_bridge::DeviceElem`: `i16`/`i8` are
// `bytemuck::Pod`, and (under the `cuda` feature) `cudarc::driver`
// implements `DeviceRepr`/`ValidAsZeroBits`/`Unpin` for every primitive
// integer width, `i16`/`i8` included — see `cuda_bridge::DeviceElem`'s own
// doc comment listing "i8, i32, i64, f32, f64, …".
direct_xfer!(
    upload_obj_i16,
    download_obj_i16,
    i16,
    ArrayElementType::Short,
    host_view_i16,
    write_back_i16,
    "short[]"
);
direct_xfer!(
    upload_obj_i8,
    download_obj_i8,
    i8,
    ArrayElementType::Byte,
    host_view_i8,
    write_back_i8,
    "byte[]"
);

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_gc::{GcBackend, VmHeap};
    use cratonvm_types::ClassId;
    use std::sync::atomic::AtomicU32;

    /// Test class id — primitive arrays do not depend on this carrying any
    /// real class metadata; the heap uses `element_type` to drive layout.
    const TEST_CID: ClassId = ClassId::new(1);

    fn fresh_heap() -> VmHeap {
        VmHeap::new(GcBackend::Generational, 1 << 20)
    }

    /// Construct a free-standing `SafepointToken` against a local
    /// `AtomicU32` for unit tests. In production the counter lives on
    /// `SharedVm` and `enter_gpu_critical` returns the token.
    fn fresh_token(counter: &AtomicU32) -> SafepointToken<'_> {
        SafepointToken::new(counter)
    }

    #[test]
    fn roundtrip_i32_small() {
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Int, 4);
        // Fill via the heap API.
        for (i, v) in [10_i32, -20, i32::MIN, i32::MAX].iter().enumerate() {
            heap.set_array_element(arr, i, Value::Int(*v)).unwrap();
        }
        let view = host_view_i32(arr, &heap, &token);
        assert_eq!(view, vec![10, -20, i32::MIN, i32::MAX]);

        // Write back a new sequence and read it through the heap API.
        let next = vec![1_i32, 2, 3, 4];
        write_back_i32(arr, &heap, &next, &token);
        for (i, expected) in next.iter().enumerate() {
            let v = heap.get_array_element(arr, i).unwrap();
            assert_eq!(v, Value::Int(*expected));
        }
    }

    #[test]
    fn roundtrip_i32_large_1024() {
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let n = 1024usize;
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Int, n);
        // Sequence: i * 7 - 3 — exercises sign and stride.
        let src: Vec<i32> = (0..n as i32)
            .map(|i| i.wrapping_mul(7).wrapping_sub(3))
            .collect();
        write_back_i32(arr, &heap, &src, &token);
        let view = host_view_i32(arr, &heap, &token);
        assert_eq!(view.len(), n);
        assert_eq!(view, src);

        // Mutate the host view and write back; ensure the heap reflects it.
        let mutated: Vec<i32> = view.iter().map(|x| !x).collect();
        write_back_i32(arr, &heap, &mutated, &token);
        for (i, expected) in mutated.iter().enumerate() {
            assert_eq!(
                heap.get_array_element(arr, i).unwrap(),
                Value::Int(*expected)
            );
        }
    }

    #[test]
    fn roundtrip_i64() {
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Long, 5);
        let src = vec![0_i64, 1, -1, i64::MIN, i64::MAX];
        write_back_i64(arr, &heap, &src, &token);
        assert_eq!(host_view_i64(arr, &heap, &token), src);
        // Update via heap API and ensure host view changes.
        heap.set_array_element(arr, 2, Value::Long(0xDEAD_BEEF_CAFE_F00D_u64 as i64))
            .unwrap();
        let v = host_view_i64(arr, &heap, &token);
        assert_eq!(v[2], 0xDEAD_BEEF_CAFE_F00D_u64 as i64);
    }

    #[test]
    fn roundtrip_f32() {
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Float, 6);
        let src = vec![0.0_f32, -0.0, 1.5, -3.25, f32::INFINITY, f32::NEG_INFINITY];
        write_back_f32(arr, &heap, &src, &token);
        // Compare bitwise so -0.0 and 0.0 are distinguished.
        let view = host_view_f32(arr, &heap, &token);
        assert_eq!(view.len(), src.len());
        for (a, b) in view.iter().zip(src.iter()) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn roundtrip_f64() {
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Double, 3);
        let src = vec![1.0_f64, std::f64::consts::PI, f64::EPSILON];
        write_back_f64(arr, &heap, &src, &token);
        let view = host_view_f64(arr, &heap, &token);
        for (a, b) in view.iter().zip(src.iter()) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn roundtrip_i16() {
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Short, 4);
        let src: Vec<i16> = vec![0, -1, i16::MIN, i16::MAX];
        write_back_i16(arr, &heap, &src, &token);
        assert_eq!(host_view_i16(arr, &heap, &token), src);
    }

    #[test]
    fn roundtrip_i8() {
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Byte, 5);
        let src: Vec<i8> = vec![0, -1, 1, i8::MIN, i8::MAX];
        write_back_i8(arr, &heap, &src, &token);
        assert_eq!(host_view_i8(arr, &heap, &token), src);
    }

    #[test]
    fn roundtrip_i16_bulk_odd_length() {
        // Odd element count over the 2-byte-stride bulk `copy_nonoverlapping`
        // path — pins down that the byte count (`len * 2`) is computed from
        // the *element* count, not rounded to some even/aligned quantity.
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let n = 257usize; // odd, and > any plausible SIMD-chunk width
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Short, n);
        let src: Vec<i16> = (0..n as i32)
            .map(|i| i.wrapping_mul(9173).wrapping_sub(31) as i16)
            .collect();
        write_back_i16(arr, &heap, &src, &token);
        let view = host_view_i16(arr, &heap, &token);
        assert_eq!(view, src);
        // Cross-check the tail element through the value-based accessor too,
        // so a bulk path that silently drops/misaligns the last element
        // would be caught even if `view == src` had a matching-length bug.
        assert_eq!(
            heap.get_array_element(arr, n - 1).unwrap(),
            Value::Int(src[n - 1] as i32)
        );
    }

    #[test]
    fn roundtrip_i16_zero_length() {
        // `len == 0` must skip the bulk-copy branch entirely (no zero-byte
        // `copy_nonoverlapping` from a possibly-dangling `array_data_ptr`)
        // and round-trip to an empty Vec without panicking.
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Short, 0);
        write_back_i16(arr, &heap, &[], &token);
        assert!(host_view_i16(arr, &heap, &token).is_empty());
    }

    #[test]
    fn roundtrip_i8_bulk_odd_length() {
        // Odd element count over the 1-byte-stride bulk path.
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let n = 513usize; // odd
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Byte, n);
        let src: Vec<i8> = (0..n as i32)
            .map(|i| i.wrapping_mul(37).wrapping_sub(5) as i8)
            .collect();
        write_back_i8(arr, &heap, &src, &token);
        let view = host_view_i8(arr, &heap, &token);
        assert_eq!(view, src);
        assert_eq!(
            heap.get_array_element(arr, n - 1).unwrap(),
            Value::Int(src[n - 1] as i32)
        );
    }

    #[test]
    fn roundtrip_i8_zero_length() {
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Byte, 0);
        write_back_i8(arr, &heap, &[], &token);
        assert!(host_view_i8(arr, &heap, &token).is_empty());
    }

    // ── Humongous-scale bulk path (G1) ──────────────────────────────────
    //
    // `VmHeap::array_data_ptr` (gc/src/vm_heap.rs) now returns `Some` for
    // EVERY array, including G1 humongous ones: the single-arena change
    // documented there made a humongous span one physically-contiguous
    // block, so the `None` per-element fallback branch in `host_view_i16` /
    // `host_view_i8` / `write_back_i16` / `write_back_i8` (and their
    // 32/64-bit siblings) is presently unreachable through any real
    // `VmHeap` backend. It is kept — per this module's "Layout note" — so
    // the marshalling code stays correct if a future heap-layout change
    // (e.g. a return to fragmented humongous regions) reintroduces a
    // non-contiguous array. Constructing that `None` case would require a
    // fake/mock `VmHeap`, which is out of this file's remit (`VmHeap` is a
    // concrete enum owned by `cratonvm-gc`, not a trait we can stub here).
    //
    // What *is* verifiable in this file: that the bulk path is correct at
    // genuinely G1-humongous scale — i.e. for an array over the
    // `region_size / 2` humongous threshold (default 1 MiB region, so
    // > 512 KiB), which is exactly the size class that used to force the
    // fallback before the arena change (see
    // `array_data_ptr_is_flat_and_contiguous_for_g1_humongous` in
    // `gc/src/vm_heap.rs`).

    #[test]
    fn roundtrip_i16_bulk_g1_humongous_scale() {
        let heap = VmHeap::new(GcBackend::G1, 4 * 1024 * 1024);
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        // 300_000 * 2 bytes = 600,000 bytes, comfortably over the 512 KiB
        // (default 1 MiB region / 2) humongous threshold.
        let n = 300_000usize;
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Short, n);
        let src: Vec<i16> = (0..n as i32)
            .map(|i| i.wrapping_mul(9173).wrapping_sub(31) as i16)
            .collect();
        write_back_i16(arr, &heap, &src, &token);
        let view = host_view_i16(arr, &heap, &token);
        assert_eq!(view.len(), n);
        assert_eq!(view, src);
        // Spot-check a tail element far past where region 0 would have
        // ended under the old fragmented-humongous layout.
        assert_eq!(
            heap.get_array_element(arr, n - 1).unwrap(),
            Value::Int(src[n - 1] as i32)
        );
    }

    #[test]
    fn roundtrip_i8_bulk_g1_humongous_scale() {
        let heap = VmHeap::new(GcBackend::G1, 4 * 1024 * 1024);
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        // 600_000 bytes, comfortably over the 512 KiB humongous threshold.
        let n = 600_000usize;
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Byte, n);
        let src: Vec<i8> = (0..n as i32)
            .map(|i| i.wrapping_mul(37).wrapping_sub(5) as i8)
            .collect();
        write_back_i8(arr, &heap, &src, &token);
        let view = host_view_i8(arr, &heap, &token);
        assert_eq!(view.len(), n);
        assert_eq!(view, src);
        assert_eq!(
            heap.get_array_element(arr, n - 1).unwrap(),
            Value::Int(src[n - 1] as i32)
        );
    }

    #[test]
    #[should_panic(expected = "src length")]
    fn write_back_length_mismatch_panics() {
        let heap = fresh_heap();
        let counter = AtomicU32::new(0);
        let token = fresh_token(&counter);
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Int, 3);
        let src = vec![1_i32, 2]; // wrong length on purpose
        write_back_i32(arr, &heap, &src, &token);
    }

    #[test]
    fn zerocopy_shape_ok_accepts_matching_element_type() {
        // The zero-copy direct-transfer fast path keys off this gate before
        // reinterpreting the heap arena as a typed slice. It must accept an
        // array only when both the kind *and* the element type line up.
        let heap = fresh_heap();
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Int, 4);
        assert!(zerocopy_shape_ok(arr, &heap, ArrayElementType::Int));
    }

    #[test]
    fn zerocopy_shape_ok_rejects_element_type_mismatch() {
        // An `int[]` reinterpreted as `long[]` would read 8 bytes per element
        // off a 4-byte-per-element payload → OOB read / heap corruption. The
        // gate must reject it so the caller falls back to the staged path.
        let heap = fresh_heap();
        let arr = heap.alloc_array(TEST_CID, ArrayElementType::Int, 4);
        assert!(!zerocopy_shape_ok(arr, &heap, ArrayElementType::Long));
        assert!(!zerocopy_shape_ok(arr, &heap, ArrayElementType::Double));
        assert!(!zerocopy_shape_ok(arr, &heap, ArrayElementType::Float));
    }

    #[test]
    fn zerocopy_shape_ok_rejects_non_array_object() {
        // A non-array object whose bytes happen to alias the `array_length`
        // slot must not be treated as a typed array by the zero-copy path.
        let heap = fresh_heap();
        let obj = heap.alloc_object(TEST_CID, 2);
        assert!(!zerocopy_shape_ok(obj, &heap, ArrayElementType::Int));
    }

    #[test]
    fn zerocopy_shape_ok_accepts_short_and_byte() {
        // Same gate, exercised for the two element types this change adds
        // to the zero-copy `direct_xfer!` set (`upload_obj_i16`/`_i8` and
        // their `download_obj_*` counterparts).
        let heap = fresh_heap();
        let shorts = heap.alloc_array(TEST_CID, ArrayElementType::Short, 4);
        assert!(zerocopy_shape_ok(shorts, &heap, ArrayElementType::Short));
        let bytes = heap.alloc_array(TEST_CID, ArrayElementType::Byte, 4);
        assert!(zerocopy_shape_ok(bytes, &heap, ArrayElementType::Byte));
    }

    #[test]
    fn zerocopy_shape_ok_rejects_short_byte_cross_mismatch() {
        // A `short[]` reinterpreted as `byte[]` (or vice versa) would read
        // the wrong byte count per element — same OOB hazard as the
        // int/long/float/double cross-checks above, now for the narrow
        // types.
        let heap = fresh_heap();
        let shorts = heap.alloc_array(TEST_CID, ArrayElementType::Short, 4);
        assert!(!zerocopy_shape_ok(shorts, &heap, ArrayElementType::Byte));
        assert!(!zerocopy_shape_ok(shorts, &heap, ArrayElementType::Int));
        let bytes = heap.alloc_array(TEST_CID, ArrayElementType::Byte, 4);
        assert!(!zerocopy_shape_ok(bytes, &heap, ArrayElementType::Short));
    }

    #[test]
    fn upload_without_driver_returns_no_driver() {
        // On this machine (no CUDA toolkit, default-features build of
        // cuda-bridge) any device-side entry point must return NoDriver.
        // We rely on that to keep this test deterministic.
        let ctx_err = DeviceContext::new(0).err();
        match ctx_err {
            Some(DeviceError::NoDriver) => {}
            Some(other) => panic!("expected NoDriver, got {other:?}"),
            None => panic!("expected NoDriver, got Ok (machine has a GPU?)"),
        }
    }
}

/// Copy `src` into `obj`'s payload starting at element `lo`.
///
/// The ranged twin of the whole-array `write_back_*`, for the chunked
/// writeback: each chunk lands in its own slice of the Java array as its
/// DMA completes, rather than the whole array arriving at once. Same
/// no-write-barrier argument (primitive arrays never hold references) and
/// same contiguity fallback.
macro_rules! write_back_range {
    ($name:ident, $ty:ty, $elem:expr, $vconv:expr, $what:literal) => {
        pub fn $name(
            obj: ObjectRef,
            heap: &VmHeap,
            src: &[$ty],
            lo: usize,
            _token: &SafepointToken<'_>,
        ) -> Result<(), String> {
            let header = heap.get_header(obj);
            if header.kind() != ObjectKind::Array || header.element_type() != $elem {
                return Err(concat!(stringify!($name), ": not a ", $what).to_string());
            }
            let len = header.array_length() as usize;
            let end = lo
                .checked_add(src.len())
                .ok_or_else(|| format!("{}: lo + len overflows", stringify!($name)))?;
            if end > len {
                return Err(format!(
                    "{}: range [{lo}, {end}) exceeds array length {len}",
                    stringify!($name)
                ));
            }
            if src.is_empty() {
                return Ok(());
            }
            match heap.array_data_ptr(obj) {
                Some(dst) => {
                    // SAFETY: `obj` is a live `$what` of `len` elements
                    // (kind + element type checked above), the range is
                    // bounds-checked against it, and GC is paused for the
                    // duration (token held), so `dst` stays valid. `src` is
                    // page-locked staging owned by the caller and cannot
                    // overlap the heap arena.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            src.as_ptr() as *const u8,
                            dst.add(lo * std::mem::size_of::<$ty>()),
                            std::mem::size_of_val(src),
                        );
                    }
                }
                // G1 humongous: region-safe per-element store.
                None => {
                    for (i, &x) in src.iter().enumerate() {
                        heap.set_array_element(obj, lo + i, $vconv(x))
                            .map_err(|_| format!("{}: OOB at {}", stringify!($name), lo + i))?;
                    }
                }
            }
            Ok(())
        }
    };
}

write_back_range!(write_back_range_i32, i32, ArrayElementType::Int, Value::Int, "int[]");
write_back_range!(write_back_range_i64, i64, ArrayElementType::Long, Value::Long, "long[]");
write_back_range!(write_back_range_f32, f32, ArrayElementType::Float, Value::Float, "float[]");
write_back_range!(
    write_back_range_f64,
    f64,
    ArrayElementType::Double,
    Value::Double,
    "double[]"
);
