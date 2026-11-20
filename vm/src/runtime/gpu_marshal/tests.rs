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
