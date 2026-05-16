//! Part C — primitive-array marshalling between the JVM heap and CUDA device
//! memory.
//!
//! The whole module is gated behind the `gpu-offload` Cargo feature on
//! `rustjvm-vm`. With the feature off, this file is not compiled and no
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
//! `rustjvm-gc` layout, primitive arrays in fact use **native element
//! sizes** (`element_byte_size` in `rustjvm_types::heap_types`): 4 bytes
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

use rustjvm_gc::safepoint::SafepointToken;
use rustjvm_gc::VmHeap;
use rustjvm_types::{ArrayElementType, ObjectKind, ObjectRef, Value};

pub use cuda_bridge::{DeviceBuffer, DeviceContext, DeviceError, Result as DeviceResult};

// ── Host views (heap → packed Vec<T>) ────────────────────────────────────

/// Copy a Java `int[]` out of the JVM heap into a packed host `Vec<i32>`.
///
/// # Panics
///
/// Panics if `obj` is not a heap-allocated `int[]`. This is an internal
/// invariant — Part D's analyzer + Part E's caller verify the array shape
/// before reaching this point.
pub fn host_view_i32(
    obj: ObjectRef,
    heap: &VmHeap,
    _token: &SafepointToken<'_>,
) -> Vec<i32> {
    let header = heap.get_header(obj);
    assert_eq!(header.kind, ObjectKind::Array, "host_view_i32: not an array");
    assert_eq!(
        header.element_type,
        ArrayElementType::Int,
        "host_view_i32: not an int[]"
    );
    let len = header.array_length as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        // SAFETY of unwrap: i < array_length by construction.
        let v = heap
            .get_array_element(obj, i)
            .expect("host_view_i32: in-bounds index returned OOB");
        match v {
            Value::Int(x) => out.push(x),
            other => panic!("host_view_i32: expected Value::Int, got {other:?}"),
        }
    }
    out
}

/// Copy a Java `long[]` into a packed host `Vec<i64>`.
pub fn host_view_i64(
    obj: ObjectRef,
    heap: &VmHeap,
    _token: &SafepointToken<'_>,
) -> Vec<i64> {
    let header = heap.get_header(obj);
    assert_eq!(header.kind, ObjectKind::Array, "host_view_i64: not an array");
    assert_eq!(
        header.element_type,
        ArrayElementType::Long,
        "host_view_i64: not a long[]"
    );
    let len = header.array_length as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let v = heap
            .get_array_element(obj, i)
            .expect("host_view_i64: in-bounds index returned OOB");
        match v {
            Value::Long(x) => out.push(x),
            other => panic!("host_view_i64: expected Value::Long, got {other:?}"),
        }
    }
    out
}

/// Copy a Java `float[]` into a packed host `Vec<f32>`.
pub fn host_view_f32(
    obj: ObjectRef,
    heap: &VmHeap,
    _token: &SafepointToken<'_>,
) -> Vec<f32> {
    let header = heap.get_header(obj);
    assert_eq!(header.kind, ObjectKind::Array, "host_view_f32: not an array");
    assert_eq!(
        header.element_type,
        ArrayElementType::Float,
        "host_view_f32: not a float[]"
    );
    let len = header.array_length as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let v = heap
            .get_array_element(obj, i)
            .expect("host_view_f32: in-bounds index returned OOB");
        match v {
            Value::Float(x) => out.push(x),
            other => panic!("host_view_f32: expected Value::Float, got {other:?}"),
        }
    }
    out
}

/// Copy a Java `double[]` into a packed host `Vec<f64>`.
pub fn host_view_f64(
    obj: ObjectRef,
    heap: &VmHeap,
    _token: &SafepointToken<'_>,
) -> Vec<f64> {
    let header = heap.get_header(obj);
    assert_eq!(header.kind, ObjectKind::Array, "host_view_f64: not an array");
    assert_eq!(
        header.element_type,
        ArrayElementType::Double,
        "host_view_f64: not a double[]"
    );
    let len = header.array_length as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let v = heap
            .get_array_element(obj, i)
            .expect("host_view_f64: in-bounds index returned OOB");
        match v {
            Value::Double(x) => out.push(x),
            other => panic!("host_view_f64: expected Value::Double, got {other:?}"),
        }
    }
    out
}

/// Copy a Java `short[]` into a packed host `Vec<i16>`. Heap storage
/// sign-extends to `Value::Int`; we truncate back to `i16` on the way out.
pub fn host_view_i16(
    obj: ObjectRef,
    heap: &VmHeap,
    _token: &SafepointToken<'_>,
) -> Vec<i16> {
    let header = heap.get_header(obj);
    assert_eq!(header.kind, ObjectKind::Array, "host_view_i16: not an array");
    assert_eq!(
        header.element_type,
        ArrayElementType::Short,
        "host_view_i16: not a short[]"
    );
    let len = header.array_length as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let v = heap
            .get_array_element(obj, i)
            .expect("host_view_i16: in-bounds index returned OOB");
        match v {
            Value::Int(x) => out.push(x as i16),
            other => panic!("host_view_i16: expected Value::Int, got {other:?}"),
        }
    }
    out
}

/// Copy a Java `byte[]` into a packed host `Vec<i8>`. Heap storage
/// sign-extends to `Value::Int`; we truncate to `i8` on the way out.
pub fn host_view_i8(
    obj: ObjectRef,
    heap: &VmHeap,
    _token: &SafepointToken<'_>,
) -> Vec<i8> {
    let header = heap.get_header(obj);
    assert_eq!(header.kind, ObjectKind::Array, "host_view_i8: not an array");
    assert_eq!(
        header.element_type,
        ArrayElementType::Byte,
        "host_view_i8: not a byte[]"
    );
    let len = header.array_length as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let v = heap
            .get_array_element(obj, i)
            .expect("host_view_i8: in-bounds index returned OOB");
        match v {
            Value::Int(x) => out.push(x as i8),
            other => panic!("host_view_i8: expected Value::Int, got {other:?}"),
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
pub fn write_back_i32(
    obj: ObjectRef,
    heap: &VmHeap,
    src: &[i32],
    _token: &SafepointToken<'_>,
) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind,
        ObjectKind::Array,
        "write_back_i32: not an array"
    );
    assert_eq!(
        header.element_type,
        ArrayElementType::Int,
        "write_back_i32: not an int[]"
    );
    let len = header.array_length as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i32: src length {} != array length {}",
        src.len(),
        len
    );
    for (i, &v) in src.iter().enumerate() {
        heap.set_array_element(obj, i, Value::Int(v))
            .expect("write_back_i32: in-bounds index returned OOB");
    }
}

/// Copy a packed host `&[i64]` into a JVM `long[]`.
pub fn write_back_i64(
    obj: ObjectRef,
    heap: &VmHeap,
    src: &[i64],
    _token: &SafepointToken<'_>,
) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind,
        ObjectKind::Array,
        "write_back_i64: not an array"
    );
    assert_eq!(
        header.element_type,
        ArrayElementType::Long,
        "write_back_i64: not a long[]"
    );
    let len = header.array_length as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i64: src length {} != array length {}",
        src.len(),
        len
    );
    for (i, &v) in src.iter().enumerate() {
        heap.set_array_element(obj, i, Value::Long(v))
            .expect("write_back_i64: in-bounds index returned OOB");
    }
}

/// Copy a packed host `&[f32]` into a JVM `float[]`.
pub fn write_back_f32(
    obj: ObjectRef,
    heap: &VmHeap,
    src: &[f32],
    _token: &SafepointToken<'_>,
) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind,
        ObjectKind::Array,
        "write_back_f32: not an array"
    );
    assert_eq!(
        header.element_type,
        ArrayElementType::Float,
        "write_back_f32: not a float[]"
    );
    let len = header.array_length as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_f32: src length {} != array length {}",
        src.len(),
        len
    );
    for (i, &v) in src.iter().enumerate() {
        heap.set_array_element(obj, i, Value::Float(v))
            .expect("write_back_f32: in-bounds index returned OOB");
    }
}

/// Copy a packed host `&[f64]` into a JVM `double[]`.
pub fn write_back_f64(
    obj: ObjectRef,
    heap: &VmHeap,
    src: &[f64],
    _token: &SafepointToken<'_>,
) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind,
        ObjectKind::Array,
        "write_back_f64: not an array"
    );
    assert_eq!(
        header.element_type,
        ArrayElementType::Double,
        "write_back_f64: not a double[]"
    );
    let len = header.array_length as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_f64: src length {} != array length {}",
        src.len(),
        len
    );
    for (i, &v) in src.iter().enumerate() {
        heap.set_array_element(obj, i, Value::Double(v))
            .expect("write_back_f64: in-bounds index returned OOB");
    }
}

/// Copy a packed host `&[i16]` into a JVM `short[]`. Short slots on the
/// heap are 2 bytes; `set_array_element` truncates `Value::Int` to `i16`
/// on write.
pub fn write_back_i16(
    obj: ObjectRef,
    heap: &VmHeap,
    src: &[i16],
    _token: &SafepointToken<'_>,
) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind,
        ObjectKind::Array,
        "write_back_i16: not an array"
    );
    assert_eq!(
        header.element_type,
        ArrayElementType::Short,
        "write_back_i16: not a short[]"
    );
    let len = header.array_length as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i16: src length {} != array length {}",
        src.len(),
        len
    );
    for (i, &v) in src.iter().enumerate() {
        heap.set_array_element(obj, i, Value::Int(v as i32))
            .expect("write_back_i16: in-bounds index returned OOB");
    }
}

/// Copy a packed host `&[i8]` into a JVM `byte[]`. Byte slots on the
/// heap are 1 byte; `set_array_element` truncates `Value::Int` to `u8`
/// on write.
pub fn write_back_i8(
    obj: ObjectRef,
    heap: &VmHeap,
    src: &[i8],
    _token: &SafepointToken<'_>,
) {
    let header = heap.get_header(obj);
    assert_eq!(
        header.kind,
        ObjectKind::Array,
        "write_back_i8: not an array"
    );
    assert_eq!(
        header.element_type,
        ArrayElementType::Byte,
        "write_back_i8: not a byte[]"
    );
    let len = header.array_length as usize;
    assert_eq!(
        src.len(),
        len,
        "write_back_i8: src length {} != array length {}",
        src.len(),
        len
    );
    for (i, &v) in src.iter().enumerate() {
        heap.set_array_element(obj, i, Value::Int(v as i32))
            .expect("write_back_i8: in-bounds index returned OOB");
    }
}

// ── Device buffer transfer wrappers ──────────────────────────────────────

/// Allocate a device buffer and upload `host` into it in one shot.
///
/// On a machine without a CUDA driver (or when `cuda-bridge` was built
/// without the `cuda` feature), this returns `Err(DeviceError::NoDriver)`.
pub fn upload<T>(ctx: &DeviceContext, host: &[T]) -> DeviceResult<DeviceBuffer<T>>
where
    T: cuda_bridge::bytemuck::Pod + Send + Sync + 'static,
{
    DeviceBuffer::from_host(ctx, host)
}

/// Copy `buf`'s contents back into `dst`. `dst.len()` must be at least
/// `buf.len()`; `cuda-bridge` enforces this at the driver layer.
pub fn download_into<T>(buf: &DeviceBuffer<T>, dst: &mut [T]) -> DeviceResult<()>
where
    T: cuda_bridge::bytemuck::Pod + Send + Sync + 'static,
{
    buf.to_host(dst)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rustjvm_gc::{GcBackend, VmHeap};
    use rustjvm_types::ClassId;
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
        let src: Vec<i32> = (0..n as i32).map(|i| i.wrapping_mul(7).wrapping_sub(3)).collect();
        write_back_i32(arr, &heap, &src, &token);
        let view = host_view_i32(arr, &heap, &token);
        assert_eq!(view.len(), n);
        assert_eq!(view, src);

        // Mutate the host view and write back; ensure the heap reflects it.
        let mutated: Vec<i32> = view.iter().map(|x| !x).collect();
        write_back_i32(arr, &heap, &mutated, &token);
        for (i, expected) in mutated.iter().enumerate() {
            assert_eq!(heap.get_array_element(arr, i).unwrap(), Value::Int(*expected));
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
