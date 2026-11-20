use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// The per-element answer the intrinsics give without the switch: the
/// first index whose `width`-byte elements differ, `-1` when none.
fn per_element(
    a: &[u8],
    a_byte: usize,
    b: &[u8],
    b_byte: usize,
    n: usize,
    width: usize,
) -> i32 {
    for i in 0..n {
        let x = &a[a_byte + i * width..a_byte + (i + 1) * width];
        let y = &b[b_byte + i * width..b_byte + (i + 1) * width];
        if x != y {
            return i as i32;
        }
    }
    -1
}

fn bulk(
    a: &[u8],
    a_byte: usize,
    b: &[u8],
    b_byte: usize,
    n: usize,
    width: usize,
) -> Option<i32> {
    mismatch_bulk_with(
        &mut |side_b: bool, off: usize, dst: &mut [u8]| -> usize {
            let src = if side_b { b } else { a };
            let avail = src.len().saturating_sub(off).min(dst.len());
            dst[..avail].copy_from_slice(&src[off..off + avail]);
            avail
        },
        a_byte,
        b_byte,
        n,
        width,
    )
}

/// Interpreter round i3 wave 74, lane L5 (`CRATONVM_ARRAYS_MISMATCH_BULK`):
/// the windowed byte compare answers exactly the per-element loop's index
/// for every element width, across window boundaries, at offsets, for a
/// difference in any byte of an element, for equal and empty ranges.
#[test]
fn i74_bulk_mismatch_answers_the_per_element_index() {
    let base: Vec<u8> = (0..9000u32).map(|i| ((i * 31 + 7) % 251) as u8).collect();
    for width in [1usize, 2, 4, 8] {
        let n_max = (base.len() - 16) / width;
        for n in [0usize, 1, 7, 1023, 1024, 1025, n_max] {
            // Equal.
            assert_eq!(bulk(&base, 0, &base, 0, n, width), Some(-1));
            // One differing byte at a spread of positions, every byte of an element.
            for &byte in &[0usize, 1, 2047, 2048, 2049, 4095, 4096, 6001, n * width] {
                if byte >= n * width {
                    continue;
                }
                let mut other = base.clone();
                other[byte] ^= 0x80;
                let want = per_element(&base, 0, &other, 0, n, width);
                assert_eq!(want, (byte / width) as i32);
                assert_eq!(
                    bulk(&base, 0, &other, 0, n, width),
                    Some(want),
                    "w={width} n={n} byte={byte}"
                );
                assert_eq!(bulk(&other, 0, &base, 0, n, width), Some(want));
            }
        }
        // Ranged: the same bytes at different offsets on each side.
        let n = 3000 / width;
        let mut shifted = vec![0xEEu8; 8 * width];
        shifted.extend_from_slice(&base);
        assert_eq!(bulk(&base, 0, &shifted, 8 * width, n, width), Some(-1));
        shifted[8 * width + 2500] ^= 1;
        assert_eq!(
            bulk(&base, 0, &shifted, 8 * width, n, width),
            Some(per_element(&base, 0, &shifted, 8 * width, n, width))
        );
    }
}

/// A read that comes back short (no flat payload) answers `None`, so the
/// caller keeps its per-element loop; so does an element width that does
/// not divide the window.
#[test]
fn i74_bulk_mismatch_declines_a_short_read() {
    let a = vec![1u8; 100];
    let b = vec![1u8; 50];
    assert_eq!(bulk(&a, 0, &b, 0, 100, 1), None);
    assert_eq!(bulk(&a, 0, &a, 0, 10, 3), None);
    assert_eq!(bulk(&a, 0, &a, 0, 10, 0), None);
}

/// With the switch off (the mock's default) the registered intrinsics
/// still answer through the per-element loop: `ArraysSupport.mismatch`
/// over two equal byte arrays and over one differing at index 5.
#[test]
fn i74_mismatch_intrinsics_answer_unchanged_with_the_switch_off() {
    let mut ctx = crate::test_utils::mock_ctx();
    let a = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 12);
    let b = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 12);
    for i in 0..12 {
        ctx.set_array_element(a, i, Value::Int(i as i32));
        ctx.set_array_element(b, i, Value::Int(i as i32));
    }
    assert_eq!(mismatch_bulk(&mut ctx, a, 0, b, 0, 12, 1), None);
    let mut r = NativeMethodRegistry::new();
    r.set_jit_enabled(false);
    register_arrays_support_natives(&mut r);
    let f = r
        .find("jdk/internal/util/ArraysSupport", "mismatch", "([B[BI)I")
        .expect("mismatch([B[BI)I registered");
    let args = [
        Value::Object(Some(a)),
        Value::Object(Some(b)),
        Value::Int(12),
    ];
    assert_eq!(f(&mut ctx, &args).unwrap(), Some(Value::Int(-1)));
    ctx.set_array_element(b, 5, Value::Int(-3));
    assert_eq!(f(&mut ctx, &args).unwrap(), Some(Value::Int(5)));
}
