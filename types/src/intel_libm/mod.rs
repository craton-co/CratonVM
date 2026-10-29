// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The Intel LIBM routines HotSpot's x86-64 stubs run, reproduced bit for bit.
//!
//! HotSpot 25 answers `Math.pow` on x86-64 from `StubRoutines::dpow`, Intel's
//! LIBM `pow` in its interpreter, C1 and C2 alike. That algorithm is not
//! correctly rounded, and no libm but its own reproduces it: glibc rounds the
//! near-halfway `pow(0.49999999999999994, -3.0)` down a last ulp where Intel's
//! rounds up, and Intel's rounds others down where the exact result rounds up
//! (`docs/internal/fixed-bugs/jr21-conf-math-pow-last-ulp-needs-the-dpow-port-decision-FIXED-20261027.md`).
//!
//! Intel published the same routines under BSD-3-Clause in Android's bionic
//! (`libm/x86_64/e_pow.S`, Copyright (c) 2014 Intel Corporation; see
//! `THIRD-PARTY-NOTICES.md`). `tools/libm-port/asm2rust.py` translates that
//! assembly instruction for instruction into Rust over the register machine
//! below ([`Cpu`]); the generated file carries Intel's notice. On the 64.177
//! host, bionic's assembled `pow` and HotSpot 25's `Math.pow` agreed on all
//! 4,000,000 general-case rows of a mixed random sweep; the only disagreements
//! were the special rows HotSpot settles before the routine runs (`y == 2`,
//! `y == 0.5`, a unit base with a NaN or infinite exponent), which
//! `native_math_pow` keeps doing first.
//!
//! The model: `xmm` registers are SSE registers (`__m128i`), moved, shuffled
//! and masked with the SSE2 intrinsics of the same instructions (bit
//! operations, exact under any optimization); every floating-point
//! instruction runs as itself through `asm!` ([`sse`]), so NaN bits and
//! `rcpps`'s approximation are the CPU's own, as they are for the stub;
//! general registers are plain integers with the 32-bit ALU flags the routine
//! branches on; memory is the routine's stack frame and its read-only tables.

#![cfg(target_arch = "x86_64")]
#![allow(clippy::many_single_char_names)]

// Generated; `tools/libm-port/asm2rust.py` is its source of truth.
#[rustfmt::skip]
mod pow;
pub use pow::intel_pow;

use core::arch::x86_64::{
    __m128i, _mm_cvtsi128_si64, _mm_cvtsi64_si128, _mm_loadu_si128, _mm_setzero_si128,
};

/// Where the routine's read-only data sits in the modelled address space.
pub(crate) const IMAGE_BASE: u64 = 0x1000_0000_0000;
/// The routine's frame: `rsp` after its `subq $40, %rsp`, up to its return
/// address.
const FRAME_LEN: usize = 48;

/// The register file, flags and frame of one call.
pub(crate) struct Cpu {
    /// `xmm0`..`xmm7`.
    pub x: [__m128i; 8],
    /// `rax`, `rcx`, `rdx`, `rbx`, `rsp`, `rbp`, `rsi`, `rdi`, `r8`..`r15`.
    pub g: [u64; 16],
    pub zf: bool,
    pub sf: bool,
    pub cf: bool,
    pub of: bool,
    frame: [u8; FRAME_LEN],
}

/// The low 64 bits of `x`.
#[inline(always)]
pub(crate) fn lo64(x: __m128i) -> u64 {
    // SAFETY: SSE2 is the x86-64 baseline. Cast: the register's bits.
    unsafe { _mm_cvtsi128_si64(x) as u64 }
}

/// `v` in the low 64 bits, the rest zero (`movq`).
#[inline(always)]
pub(crate) fn from64(v: u64) -> __m128i {
    // SAFETY: SSE2 is the x86-64 baseline. Cast: the register's bits.
    unsafe { _mm_cvtsi64_si128(v as i64) }
}

impl Cpu {
    /// The state at entry: `x` in `xmm0`, `y` in `xmm1`.
    #[inline(always)]
    pub(crate) fn new(x: f64, y: f64) -> Self {
        // SAFETY: SSE2 is the x86-64 baseline.
        let zero = unsafe { _mm_setzero_si128() };
        let mut c = Cpu {
            x: [zero; 8],
            g: [0; 16],
            zf: false,
            sf: false,
            cf: false,
            of: false,
            frame: [0; FRAME_LEN],
        };
        c.x[0] = from64(x.to_bits());
        c.x[1] = from64(y.to_bits());
        c
    }

    /// The `N` frame bytes at `disp(%rsp)` as a little-endian integer.
    #[inline(always)]
    pub(crate) fn frame_ld<const N: usize>(&self, disp: usize) -> u64 {
        let mut b = [0u8; 8];
        match self.frame.get(disp..disp + N) {
            Some(s) => b[..N].copy_from_slice(s),
            None => debug_assert!(false, "intel_libm: frame load at {disp}"),
        }
        u64::from_le_bytes(b)
    }

    /// Store the low `N` bytes of `v` at `disp(%rsp)`.
    #[inline(always)]
    pub(crate) fn frame_st<const N: usize>(&mut self, disp: usize, v: u64) {
        match self.frame.get_mut(disp..disp + N) {
            Some(s) => s.copy_from_slice(&v.to_le_bytes()[..N]),
            None => debug_assert!(false, "intel_libm: frame store at {disp}"),
        }
    }

    /// ZF and SF of a `BITS`-wide result.
    #[inline(always)]
    fn set_zs<const BITS: u32>(&mut self, r: u32) {
        self.zf = r == 0;
        self.sf = (r >> (BITS - 1)) & 1 == 1;
    }

    #[inline(always)]
    fn set_zs32(&mut self, r: u32) {
        self.set_zs::<32>(r);
    }

    #[inline(always)]
    pub(crate) fn add32(&mut self, a: u32, b: u32) -> u32 {
        let (r, carry) = a.overflowing_add(b);
        self.cf = carry;
        self.of = ((a ^ r) & (b ^ r)) >> 31 == 1;
        self.set_zs32(r);
        r
    }

    #[inline(always)]
    pub(crate) fn sub32(&mut self, a: u32, b: u32) -> u32 {
        let r = a.wrapping_sub(b);
        self.cf = a < b;
        self.of = ((a ^ b) & (a ^ r)) >> 31 == 1;
        self.set_zs32(r);
        r
    }

    #[inline(always)]
    fn logic32(&mut self, r: u32) -> u32 {
        self.cf = false;
        self.of = false;
        self.set_zs32(r);
        r
    }

    #[inline(always)]
    pub(crate) fn and32(&mut self, a: u32, b: u32) -> u32 {
        self.logic32(a & b)
    }

    #[inline(always)]
    pub(crate) fn or32(&mut self, a: u32, b: u32) -> u32 {
        self.logic32(a | b)
    }

    #[inline(always)]
    pub(crate) fn xor32(&mut self, a: u32, b: u32) -> u32 {
        self.logic32(a ^ b)
    }

    #[inline(always)]
    fn set_zs16(&mut self, r: u16) {
        self.set_zs::<16>(u32::from(r));
    }

    #[inline(always)]
    pub(crate) fn sub16(&mut self, a: u16, b: u16) -> u16 {
        let r = a.wrapping_sub(b);
        self.cf = a < b;
        self.of = ((a ^ b) & (a ^ r)) >> 15 == 1;
        self.set_zs16(r);
        r
    }

    #[inline(always)]
    pub(crate) fn and16(&mut self, a: u16, b: u16) -> u16 {
        let r = a & b;
        self.cf = false;
        self.of = false;
        self.set_zs16(r);
        r
    }

    /// `shl r32, n`: a masked count of 0 changes nothing, flags included.
    #[inline(always)]
    pub(crate) fn shl32(&mut self, a: u32, n: u32) -> u32 {
        let n = n & 31;
        if n == 0 {
            return a;
        }
        let r = a << n;
        self.cf = (a >> (32 - n)) & 1 == 1;
        self.of = (r >> 31 == 1) != self.cf;
        self.set_zs32(r);
        r
    }

    #[inline(always)]
    pub(crate) fn shr32(&mut self, a: u32, n: u32) -> u32 {
        let n = n & 31;
        if n == 0 {
            return a;
        }
        let r = a >> n;
        self.cf = (a >> (n - 1)) & 1 == 1;
        self.of = a >> 31 == 1;
        self.set_zs32(r);
        r
    }

    #[inline(always)]
    pub(crate) fn sar32(&mut self, a: u32, n: u32) -> u32 {
        let n = n & 31;
        if n == 0 {
            return a;
        }
        // Casts: an arithmetic shift is a signed shift of the same bits.
        let s = a as i32;
        let r = (s >> n) as u32;
        self.cf = (s >> (n - 1)) & 1 == 1;
        self.of = false;
        self.set_zs32(r);
        r
    }

    /// `bsr src32, dst32`: a zero source sets ZF and leaves the destination.
    #[inline(always)]
    pub(crate) fn bsr32(&mut self, src: usize, dst: usize) {
        // Cast: the low 32 bits.
        let s = self.g[src] as u32;
        if s == 0 {
            self.zf = true;
        } else {
            self.zf = false;
            self.g[dst] = u64::from(31 - s.leading_zeros());
        }
    }
}

/// The `N` image bytes at `addr` (`N` <= 8) as a little-endian integer. An
/// address outside the image is a translation bug: debug builds stop, release
/// builds read zero.
#[inline(always)]
pub(crate) fn image_ld<const N: usize>(image: &[u8], addr: u64) -> u64 {
    let mut b = [0u8; 8];
    let off = usize::try_from(addr.wrapping_sub(IMAGE_BASE)).unwrap_or(usize::MAX);
    match off.checked_add(N).and_then(|end| image.get(off..end)) {
        Some(s) => b[..N].copy_from_slice(s),
        None => debug_assert!(false, "intel_libm: image load at {addr:#x}"),
    }
    u64::from_le_bytes(b)
}

/// The 16 image bytes at `addr`.
#[inline(always)]
pub(crate) fn image_ld128(image: &[u8], addr: u64) -> __m128i {
    let off = usize::try_from(addr.wrapping_sub(IMAGE_BASE)).unwrap_or(usize::MAX);
    match off.checked_add(16).and_then(|end| image.get(off..end)) {
        // SAFETY: the slice holds 16 readable bytes; the load is unaligned.
        Some(s) => unsafe { _mm_loadu_si128(s.as_ptr().cast::<__m128i>()) },
        None => {
            debug_assert!(false, "intel_libm: image load at {addr:#x}");
            // SAFETY: SSE2 is the x86-64 baseline.
            unsafe { _mm_setzero_si128() }
        }
    }
}

/// The routine's floating-point instructions, executed as themselves.
///
/// Rust leaves the bits of a NaN result unspecified, and LLVM constant-folds
/// and commutes float arithmetic: with the routine's constants in a `static`,
/// a release build folded the invalid operation of `pow(-3.0, 0.5x)` to
/// 7ff8000000000000 where the instruction (and HotSpot) give x86's default
/// NaN fff8000000000000 (212,603 of 2,933,711 rows of a HotSpot sweep on the
/// 64.177 host differed in a release build before this module), and a
/// commuted `addsd` of two NaNs would pass on the other operand's payload.
/// Each helper is the one instruction, opaque to the optimizer, with the
/// destination as the first operand as the routine has it; lanes the
/// instruction does not write keep the destination's bits.
pub(crate) mod sse {
    use core::arch::asm;
    use core::arch::x86_64::__m128i;

    macro_rules! sse_op {
        ($name:ident) => {
            #[inline(always)]
            pub(crate) fn $name(mut d: __m128i, s: __m128i) -> __m128i {
                // SAFETY: SSE/SSE2 are the x86-64 baseline; the instruction
                // reads and writes registers only.
                unsafe {
                    asm!(
                        concat!(stringify!($name), " {0}, {1}"),
                        inout(xmm_reg) d,
                        in(xmm_reg) s,
                        options(pure, nomem, nostack, preserves_flags),
                    );
                }
                d
            }
        };
    }

    sse_op!(addsd);
    sse_op!(subsd);
    sse_op!(mulsd);
    sse_op!(divsd);
    sse_op!(addpd);
    sse_op!(subpd);
    sse_op!(mulpd);
    sse_op!(divpd);
    sse_op!(addss);
    sse_op!(subss);
    sse_op!(mulss);

    /// `rcpps src, dst`: the CPU's own approximate reciprocal, lane by lane,
    /// as the stub computes it.
    #[inline(always)]
    pub(crate) fn rcpps(mut x: __m128i) -> __m128i {
        // SAFETY: as for the arithmetic above.
        unsafe {
            asm!(
                "rcpps {0}, {0}",
                inout(xmm_reg) x,
                options(pure, nomem, nostack, preserves_flags),
            );
        }
        x
    }

    /// `cvtsi2sd r32, xmm`: the low lane becomes the signed 32-bit value.
    #[inline(always)]
    pub(crate) fn cvtsi2sd32(mut d: __m128i, v: u32) -> __m128i {
        // Cast: the register's bits, read as signed by the instruction.
        let v = v as i32;
        // SAFETY: as for the arithmetic above.
        unsafe {
            asm!(
                "cvtsi2sd {0}, {1:e}",
                inout(xmm_reg) d,
                in(reg) v,
                options(pure, nomem, nostack, preserves_flags),
            );
        }
        d
    }
}

#[cfg(test)]
mod tests {
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
}
