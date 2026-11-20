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
mod tests;
