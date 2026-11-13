// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! AVX-512 vector register (zmm0-zmm31) and mask register (k0-k7) scrubbing.
//!
//! Non-optimizable zeroing primitives for SIMD registers after cryptographic operations.

/// Zero all AVX-512 vector registers (zmm0-zmm31) and mask registers (k0-k7).
/// Falls back to zeroing ymm0-ymm15 on AVX systems, or xmm0-xmm15 on SSE systems.
#[inline]
pub fn scrub_vector_registers() {
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx512f") {
            // SAFETY: Checked runtime support for AVX-512F.
            unsafe { scrub_avx512() };
        } else if std::is_x86_feature_detected!("avx") {
            // SAFETY: Checked runtime support for AVX.
            unsafe { scrub_avx() };
        } else {
            scrub_sse();
        }
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// Guard structure that scrubs vector registers upon drop.
pub struct ScrubOnDrop;

impl Drop for ScrubOnDrop {
    #[inline]
    fn drop(&mut self) {
        scrub_vector_registers();
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(never)]
#[target_feature(enable = "avx512f")]
unsafe fn scrub_avx512() {
    // SAFETY: zeroing registers declared as clobbers/outputs; nostack and nomem.
    // kxorw zero-extends to the full mask register width.
    unsafe {
        core::arch::asm!(
            "vpxord zmm0, zmm0, zmm0",    "vpxord zmm1, zmm1, zmm1",
            "vpxord zmm2, zmm2, zmm2",    "vpxord zmm3, zmm3, zmm3",
            "vpxord zmm4, zmm4, zmm4",    "vpxord zmm5, zmm5, zmm5",
            "vpxord zmm6, zmm6, zmm6",    "vpxord zmm7, zmm7, zmm7",
            "vpxord zmm8, zmm8, zmm8",    "vpxord zmm9, zmm9, zmm9",
            "vpxord zmm10, zmm10, zmm10", "vpxord zmm11, zmm11, zmm11",
            "vpxord zmm12, zmm12, zmm12", "vpxord zmm13, zmm13, zmm13",
            "vpxord zmm14, zmm14, zmm14", "vpxord zmm15, zmm15, zmm15",
            "vpxord zmm16, zmm16, zmm16", "vpxord zmm17, zmm17, zmm17",
            "vpxord zmm18, zmm18, zmm18", "vpxord zmm19, zmm19, zmm19",
            "vpxord zmm20, zmm20, zmm20", "vpxord zmm21, zmm21, zmm21",
            "vpxord zmm22, zmm22, zmm22", "vpxord zmm23, zmm23, zmm23",
            "vpxord zmm24, zmm24, zmm24", "vpxord zmm25, zmm25, zmm25",
            "vpxord zmm26, zmm26, zmm26", "vpxord zmm27, zmm27, zmm27",
            "vpxord zmm28, zmm28, zmm28", "vpxord zmm29, zmm29, zmm29",
            "vpxord zmm30, zmm30, zmm30", "vpxord zmm31, zmm31, zmm31",
            "kxorw k0, k0, k0", "kxorw k1, k1, k1",
            "kxorw k2, k2, k2", "kxorw k3, k3, k3",
            "kxorw k4, k4, k4", "kxorw k5, k5, k5",
            "kxorw k6, k6, k6", "kxorw k7, k7, k7",
            out("zmm0") _, out("zmm1") _, out("zmm2") _, out("zmm3") _,
            out("zmm4") _, out("zmm5") _, out("zmm6") _, out("zmm7") _,
            out("zmm8") _, out("zmm9") _, out("zmm10") _, out("zmm11") _,
            out("zmm12") _, out("zmm13") _, out("zmm14") _, out("zmm15") _,
            out("zmm16") _, out("zmm17") _, out("zmm18") _, out("zmm19") _,
            out("zmm20") _, out("zmm21") _, out("zmm22") _, out("zmm23") _,
            out("zmm24") _, out("zmm25") _, out("zmm26") _, out("zmm27") _,
            out("zmm28") _, out("zmm29") _, out("zmm30") _, out("zmm31") _,
            out("k0") _, out("k1") _, out("k2") _, out("k3") _,
            out("k4") _, out("k5") _, out("k6") _, out("k7") _,
            options(nomem, nostack, preserves_flags),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(never)]
#[target_feature(enable = "avx")]
unsafe fn scrub_avx() {
    // SAFETY: vzeroall zeroes ymm0-ymm15.
    unsafe {
        core::arch::asm!(
            "vzeroall",
            out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _,
            out("ymm4") _, out("ymm5") _, out("ymm6") _, out("ymm7") _,
            out("ymm8") _, out("ymm9") _, out("ymm10") _, out("ymm11") _,
            out("ymm12") _, out("ymm13") _, out("ymm14") _, out("ymm15") _,
            options(nomem, nostack, preserves_flags),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn scrub_sse() {
    // SAFETY: pxor zeroes xmm0-xmm15.
    unsafe {
        core::arch::asm!(
            "pxor xmm0, xmm0",   "pxor xmm1, xmm1",   "pxor xmm2, xmm2",
            "pxor xmm3, xmm3",   "pxor xmm4, xmm4",   "pxor xmm5, xmm5",
            "pxor xmm6, xmm6",   "pxor xmm7, xmm7",   "pxor xmm8, xmm8",
            "pxor xmm9, xmm9",   "pxor xmm10, xmm10", "pxor xmm11, xmm11",
            "pxor xmm12, xmm12", "pxor xmm13, xmm13", "pxor xmm14, xmm14",
            "pxor xmm15, xmm15",
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
            options(nomem, nostack, preserves_flags),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn scrub_c() {
        scrub_vector_registers();
    }

    #[test]
    fn scrub_clears_xmm1() {
        let mut buf = [0xA5u8; 16];
        unsafe {
            core::arch::asm!(
                "movdqu xmm1, [r12]",
                "sub rsp, 32",
                "call {f}",
                "add rsp, 32",
                "movdqu [r12], xmm1",
                f = sym scrub_c,
                in("r12") buf.as_mut_ptr(),
                clobber_abi("C"),
            );
        }
        assert_eq!(buf, [0u8; 16]);
    }

    #[test]
    fn scrub_clears_zmm31_and_k7() {
        if !std::is_x86_feature_detected!("avx512f") {
            return;
        }
        let mut buf = [0xA5u8; 64];
        let mut mask: u16 = 0xFFFF;
        unsafe { load_call_store_avx512(buf.as_mut_ptr(), &mut mask) };
        assert_eq!(buf, [0u8; 64]);
        assert_eq!(mask, 0);
    }

    #[target_feature(enable = "avx512f")]
    unsafe fn load_call_store_avx512(buf: *mut u8, mask: *mut u16) {
        unsafe {
            core::arch::asm!(
                "vmovdqu64 zmm31, [r12]",
                "kmovw k7, word ptr [r13]",
                "sub rsp, 32",
                "call {f}",
                "add rsp, 32",
                "vmovdqu64 [r12], zmm31",
                "kmovw word ptr [r13], k7",
                f = sym scrub_c,
                in("r12") buf,
                in("r13") mask,
                clobber_abi("C"),
            );
        }
    }

    #[test]
    fn scrub_clears_all_32_zmm_registers_and_all_mask_registers() {
        if !std::is_x86_feature_detected!("avx512f") {
            return;
        }
        let mut buf = [0xA5u8; 8 * 64];
        let mut mask: [u16; 2] = [0xFFFF, 0xFFFF];

        unsafe {
            load_call_store_bank0(buf.as_mut_ptr(), mask.as_mut_ptr());
            assert_eq!(buf, [0u8; 8 * 64], "zmm0-zmm7 must be zeroed");
            assert_eq!(mask, [0u16; 2], "k0-k1 must be zeroed");

            buf.fill(0x5Au8);
            mask.fill(0xAAAA);
            load_call_store_bank1(buf.as_mut_ptr(), mask.as_mut_ptr());
            assert_eq!(buf, [0u8; 8 * 64], "zmm8-zmm15 must be zeroed");
            assert_eq!(mask, [0u16; 2], "k2-k3 must be zeroed");

            buf.fill(0x3Cu8);
            mask.fill(0x5555);
            load_call_store_bank2(buf.as_mut_ptr(), mask.as_mut_ptr());
            assert_eq!(buf, [0u8; 8 * 64], "zmm16-zmm23 must be zeroed");
            assert_eq!(mask, [0u16; 2], "k4-k5 must be zeroed");

            buf.fill(0xF0u8);
            mask.fill(0xFFFF);
            load_call_store_bank3(buf.as_mut_ptr(), mask.as_mut_ptr());
            assert_eq!(buf, [0u8; 8 * 64], "zmm24-zmm31 must be zeroed");
            assert_eq!(mask, [0u16; 2], "k6-k7 must be zeroed");
        }
    }

    #[target_feature(enable = "avx512f")]
    unsafe fn load_call_store_bank0(buf: *mut u8, mask: *mut u16) {
        unsafe {
            core::arch::asm!(
                "vmovdqu64 zmm0, [r12 + 0*64]",
                "vmovdqu64 zmm1, [r12 + 1*64]",
                "vmovdqu64 zmm2, [r12 + 2*64]",
                "vmovdqu64 zmm3, [r12 + 3*64]",
                "vmovdqu64 zmm4, [r12 + 4*64]",
                "vmovdqu64 zmm5, [r12 + 5*64]",
                "vmovdqu64 zmm6, [r12 + 6*64]",
                "vmovdqu64 zmm7, [r12 + 7*64]",
                "kmovw k0, word ptr [r13 + 0*2]",
                "kmovw k1, word ptr [r13 + 1*2]",
                "sub rsp, 32",
                "call {f}",
                "add rsp, 32",
                "vmovdqu64 [r12 + 0*64], zmm0",
                "vmovdqu64 [r12 + 1*64], zmm1",
                "vmovdqu64 [r12 + 2*64], zmm2",
                "vmovdqu64 [r12 + 3*64], zmm3",
                "vmovdqu64 [r12 + 4*64], zmm4",
                "vmovdqu64 [r12 + 5*64], zmm5",
                "vmovdqu64 [r12 + 6*64], zmm6",
                "vmovdqu64 [r12 + 7*64], zmm7",
                "kmovw word ptr [r13 + 0*2], k0",
                "kmovw word ptr [r13 + 1*2], k1",
                f = sym scrub_c,
                in("r12") buf,
                in("r13") mask,
                clobber_abi("C"),
            );
        }
    }

    #[target_feature(enable = "avx512f")]
    unsafe fn load_call_store_bank1(buf: *mut u8, mask: *mut u16) {
        unsafe {
            core::arch::asm!(
                "vmovdqu64 zmm8, [r12 + 0*64]",
                "vmovdqu64 zmm9, [r12 + 1*64]",
                "vmovdqu64 zmm10, [r12 + 2*64]",
                "vmovdqu64 zmm11, [r12 + 3*64]",
                "vmovdqu64 zmm12, [r12 + 4*64]",
                "vmovdqu64 zmm13, [r12 + 5*64]",
                "vmovdqu64 zmm14, [r12 + 6*64]",
                "vmovdqu64 zmm15, [r12 + 7*64]",
                "kmovw k2, word ptr [r13 + 0*2]",
                "kmovw k3, word ptr [r13 + 1*2]",
                "sub rsp, 32",
                "call {f}",
                "add rsp, 32",
                "vmovdqu64 [r12 + 0*64], zmm8",
                "vmovdqu64 [r12 + 1*64], zmm9",
                "vmovdqu64 [r12 + 2*64], zmm10",
                "vmovdqu64 [r12 + 3*64], zmm11",
                "vmovdqu64 [r12 + 4*64], zmm12",
                "vmovdqu64 [r12 + 5*64], zmm13",
                "vmovdqu64 [r12 + 6*64], zmm14",
                "vmovdqu64 [r12 + 7*64], zmm15",
                "kmovw word ptr [r13 + 0*2], k2",
                "kmovw word ptr [r13 + 1*2], k3",
                f = sym scrub_c,
                in("r12") buf,
                in("r13") mask,
                clobber_abi("C"),
            );
        }
    }

    #[target_feature(enable = "avx512f")]
    unsafe fn load_call_store_bank2(buf: *mut u8, mask: *mut u16) {
        unsafe {
            core::arch::asm!(
                "vmovdqu64 zmm16, [r12 + 0*64]",
                "vmovdqu64 zmm17, [r12 + 1*64]",
                "vmovdqu64 zmm18, [r12 + 2*64]",
                "vmovdqu64 zmm19, [r12 + 3*64]",
                "vmovdqu64 zmm20, [r12 + 4*64]",
                "vmovdqu64 zmm21, [r12 + 5*64]",
                "vmovdqu64 zmm22, [r12 + 6*64]",
                "vmovdqu64 zmm23, [r12 + 7*64]",
                "kmovw k4, word ptr [r13 + 0*2]",
                "kmovw k5, word ptr [r13 + 1*2]",
                "sub rsp, 32",
                "call {f}",
                "add rsp, 32",
                "vmovdqu64 [r12 + 0*64], zmm16",
                "vmovdqu64 [r12 + 1*64], zmm17",
                "vmovdqu64 [r12 + 2*64], zmm18",
                "vmovdqu64 [r12 + 3*64], zmm19",
                "vmovdqu64 [r12 + 4*64], zmm20",
                "vmovdqu64 [r12 + 5*64], zmm21",
                "vmovdqu64 [r12 + 6*64], zmm22",
                "vmovdqu64 [r12 + 7*64], zmm23",
                "kmovw word ptr [r13 + 0*2], k4",
                "kmovw word ptr [r13 + 1*2], k5",
                f = sym scrub_c,
                in("r12") buf,
                in("r13") mask,
                clobber_abi("C"),
            );
        }
    }

    #[target_feature(enable = "avx512f")]
    unsafe fn load_call_store_bank3(buf: *mut u8, mask: *mut u16) {
        unsafe {
            core::arch::asm!(
                "vmovdqu64 zmm24, [r12 + 0*64]",
                "vmovdqu64 zmm25, [r12 + 1*64]",
                "vmovdqu64 zmm26, [r12 + 2*64]",
                "vmovdqu64 zmm27, [r12 + 3*64]",
                "vmovdqu64 zmm28, [r12 + 4*64]",
                "vmovdqu64 zmm29, [r12 + 5*64]",
                "vmovdqu64 zmm30, [r12 + 6*64]",
                "vmovdqu64 zmm31, [r12 + 7*64]",
                "kmovw k6, word ptr [r13 + 0*2]",
                "kmovw k7, word ptr [r13 + 1*2]",
                "sub rsp, 32",
                "call {f}",
                "add rsp, 32",
                "vmovdqu64 [r12 + 0*64], zmm24",
                "vmovdqu64 [r12 + 1*64], zmm25",
                "vmovdqu64 [r12 + 2*64], zmm26",
                "vmovdqu64 [r12 + 3*64], zmm27",
                "vmovdqu64 [r12 + 4*64], zmm28",
                "vmovdqu64 [r12 + 5*64], zmm29",
                "vmovdqu64 [r12 + 6*64], zmm30",
                "vmovdqu64 [r12 + 7*64], zmm31",
                "kmovw word ptr [r13 + 0*2], k6",
                "kmovw word ptr [r13 + 1*2], k7",
                f = sym scrub_c,
                in("r12") buf,
                in("r13") mask,
                clobber_abi("C"),
            );
        }
    }
}
