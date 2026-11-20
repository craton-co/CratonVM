use super::*;
use core::arch::x86_64::{_mm_extract_epi16, _mm_insert_epi16, _mm_set_epi64x, _mm_shuffle_epi32};

fn lanes(x: __m128i) -> [u64; 2] {
    // SAFETY: 16 plain bytes either way.
    unsafe { core::mem::transmute(x) }
}

fn reg(l: [u64; 2]) -> __m128i {
    // SAFETY: SSE2 is the x86-64 baseline. Casts: the lanes' bits.
    unsafe { _mm_set_epi64x(l[1] as i64, l[0] as i64) }
}

/// The intrinsics the translation names do what the instructions do.
#[test]
fn the_word_and_dword_shuffles_follow_the_instruction_set() {
    let x = reg([0x4444_3333_2222_1111, 0x8888_7777_6666_5555]);
    // SAFETY: SSE2 is the x86-64 baseline.
    unsafe {
        assert_eq!(_mm_extract_epi16::<3>(x), 0x4444);
        assert_eq!(_mm_extract_epi16::<7>(x) as u32, 0x8888);
        assert_eq!(
            lanes(_mm_insert_epi16::<5>(x, 0xEEEE)),
            [0x4444_3333_2222_1111, 0x8888_7777_EEEE_5555]
        );
        // 0b01_00_11_10: dwords 2, 3, 0, 1.
        assert_eq!(
            lanes(_mm_shuffle_epi32::<0x4E>(x)),
            [0x8888_7777_6666_5555, 0x4444_3333_2222_1111]
        );
    }
    assert_eq!(lo64(from64(7)), 7);
    assert_eq!(lanes(from64(7)), [7, 0]);
}

/// An invalid operation makes x86's default NaN, an operation on two NaNs
/// passes on the destination's, and the upper lane of a scalar op stays.
#[test]
fn the_float_ops_are_the_instructions() {
    let zero = from64(0f64.to_bits());
    assert_eq!(lo64(sse::divsd(zero, zero)), 0xfff8_0000_0000_0000);
    let n1 = reg([0x7ff8_0000_0000_0001, 7]);
    let n2 = reg([0x7ff8_0000_0000_0002, 9]);
    assert_eq!(lanes(sse::addsd(n1, n2)), [0x7ff8_0000_0000_0001, 7]);
    assert_eq!(lo64(sse::mulsd(n2, n1)), 0x7ff8_0000_0000_0002);
    let a = reg([1f64.to_bits(), 2f64.to_bits()]);
    let b = reg([3f64.to_bits(), 4f64.to_bits()]);
    assert_eq!(lanes(sse::addpd(a, b)), [4f64.to_bits(), 6f64.to_bits()]);
    assert_eq!(
        lanes(sse::cvtsi2sd32(b, u32::MAX)),
        [(-1f64).to_bits(), 4f64.to_bits()]
    );
}

#[test]
fn the_alu_flags_are_the_branches_flags() {
    let mut c = Cpu::new(0.0, 0.0);
    assert_eq!(c.sub32(1, 2), u32::MAX);
    assert!(c.cf && c.sf && !c.zf && !c.of);
    c.sub32(0x8000_0000, 1);
    assert!(c.of && !c.cf);
    c.and32(0xF0, 0x0F);
    assert!(c.zf && !c.cf);
    assert_eq!(c.sar32(0x8000_0000, 31), u32::MAX);
    c.g[1] = 0;
    c.g[2] = 7;
    c.bsr32(1, 2);
    assert!(c.zf);
    assert_eq!(c.g[2], 7, "a zero source leaves the destination");
    c.g[1] = 0x10;
    c.bsr32(1, 2);
    assert_eq!(c.g[2], 4);
}

#[test]
fn memory_is_the_frame_and_the_image() {
    let image = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    assert_eq!(image_ld::<2>(&image, IMAGE_BASE + 2), 0x0403);
    assert_eq!(lanes(image_ld128(&image, IMAGE_BASE))[1], 0x100f_0e0d_0c0b_0a09);
    let mut c = Cpu::new(0.0, 0.0);
    c.frame_st::<4>(24, 0x1122_3344);
    assert_eq!(c.frame_ld::<8>(24), 0x1122_3344);
    c.frame_st::<8>(16, u64::MAX);
    assert_eq!(c.frame_ld::<2>(22), 0xFFFF);
}
