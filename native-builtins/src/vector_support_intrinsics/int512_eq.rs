// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0

/// Returns None when AVX-512 is unavailable or its default-on master flag is vetoed.
pub(super) fn mask(a: &[i32; 16], b: &[i32; 16]) -> Option<u16> {
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_AVX512") {
        return None;
    }
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx512f") {
        let result: u32;
        // Both inputs contain all 64 bytes read; feature detection includes OS ZMM support.
        unsafe {
            std::arch::asm!(
                "vmovdqu32 zmm0, [{a}]",
                "vpcmpd k1, zmm0, [{b}], 0",
                "kmovw {result:e}, k1",
                a = in(reg) a.as_ptr(),
                b = in(reg) b.as_ptr(),
                result = out(reg) result,
                out("zmm0") _,
                out("k1") _,
                options(nostack, readonly),
            );
        }
        return Some(result as u16);
    }
    let _ = (a, b);
    None
}

#[cfg(test)]
mod tests;
