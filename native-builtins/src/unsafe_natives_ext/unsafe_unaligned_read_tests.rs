use super::{unsafe_read_bytes_into, UNSAFE_MAX_ACCESS_WIDTH};
use cratonvm_native_api::test_mock::MockNativeContext;
use cratonvm_native_api::NativeHeapAccess;
use cratonvm_types::ArrayElementType as Aet;

/// `unsafe_read_bytes_from_array`'s ABASE, restated so a test failure names
/// the offset convention rather than a bare number.
const ABASE: usize = 16;

/// **The `byte[]` bulk route and the per-element loop must answer
/// identically, at every width and every alignment.**
///
/// The bulk route is the one `HeapByteBuffer.getInt`/`getLong` take, and it
/// is selected by element size rather than by anything the caller says — so
/// nothing at the call site would notice if it disagreed with the loop it
/// replaced. `short[]` is the control: same bytes, same expected answer,
/// but two-byte elements, so it can only reach the generic loop.
#[test]
fn the_byte_array_bulk_route_agrees_with_the_per_element_loop() {
    let mut ctx = MockNativeContext::new();
    let bytes = ctx.new_array(Aet::Byte, 64);
    let shorts = ctx.new_array(Aet::Short, 32);
    // `i * 7 + 1` as a BYTE pattern, wrapping. Written inline as
    // `i as u8 * 7 + 1` this panics in a debug build from i = 37, where
    // 37 * 7 = 259 leaves `u8` -- and both loops below had it, so the test
    // could never reach the widths it was written to cover. One closure so
    // the byte[] and short[] sides cannot drift apart again.
    let b = |i: usize| -> u8 { (i as u8).wrapping_mul(7).wrapping_add(1) };
    for i in 0..64usize {
        // Cast: a byte element round-trips as a sign-extended int.
        ctx.set_array_element(bytes, i, cratonvm_types::Value::Int(i32::from(b(i) as i8)));
    }
    for i in 0..32usize {
        // The same 64 bytes, little-endian, two per element.
        let lo = u16::from(b(i * 2));
        let hi = u16::from(b(i * 2 + 1));
        ctx.set_array_element(
            shorts,
            i,
            cratonvm_types::Value::Int(i32::from((lo | (hi << 8)) as i16)),
        );
    }
    for width in [2usize, 4, 8] {
        for off in 0..8usize {
            let mut from_bytes = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
            let mut from_shorts = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
            assert!(
                unsafe_read_bytes_into(&ctx, bytes, ABASE + off, width, &mut from_bytes),
                "byte[] read refused at width {width} off {off}"
            );
            assert!(
                unsafe_read_bytes_into(&ctx, shorts, ABASE + off, width, &mut from_shorts),
                "short[] read refused at width {width} off {off}"
            );
            assert_eq!(
                from_bytes[..width],
                from_shorts[..width],
                "bulk byte[] route disagrees with the spanning loop at width {width} off {off}"
            );
        }
    }
}

/// **A read that runs off the end is refused, on both routes.**
///
/// The fast path is entered only after the same `rel + width <=
/// total_bytes` check the loop uses, so this pins that the check was not
/// moved behind it. An offset below ABASE is refused too: it is not an
/// array-element offset at all.
#[test]
fn an_out_of_range_read_is_refused_rather_than_truncated() {
    let mut ctx = MockNativeContext::new();
    let bytes = ctx.new_array(Aet::Byte, 8);
    let mut out = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
    assert!(unsafe_read_bytes_into(&ctx, bytes, ABASE, 8, &mut out));
    assert!(
        !unsafe_read_bytes_into(&ctx, bytes, ABASE + 1, 8, &mut out),
        "one byte past the end must be refused"
    );
    assert!(
        !unsafe_read_bytes_into(&ctx, bytes, ABASE - 1, 2, &mut out),
        "an offset below ABASE is not an element offset"
    );
    assert!(
        !unsafe_read_bytes_into(&ctx, bytes, ABASE, UNSAFE_MAX_ACCESS_WIDTH + 1, &mut out),
        "a width wider than the buffer must be refused, not truncated"
    );
}

/// **A reference array has no byte-addressable storage and is refused.**
/// The fast path keys on element SIZE, so this pins that the element-type
/// match still rejects `Reference` before any size is derived.
#[test]
fn a_reference_array_is_refused() {
    let mut ctx = MockNativeContext::new();
    let refs = ctx.new_array(Aet::Reference, 8);
    let mut out = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
    assert!(!unsafe_read_bytes_into(&ctx, refs, ABASE, 4, &mut out));
}
