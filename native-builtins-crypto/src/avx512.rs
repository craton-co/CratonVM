// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! AVX-512 crypto arithmetic, default enabled for testing. Feature checks include OS register support.

pub fn rsa_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_AVX512_RSA")
        && master_enabled()
        && ifma_available()
}
pub fn ec_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_AVX512_EC")
        && master_enabled()
        && ifma_available()
}
pub fn pqc_enabled() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_AVX512_PQC")
            && master_enabled()
            && std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}
pub fn aes_enabled() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_AVX512_AES")
            && master_enabled()
            && std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("vaes")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}
fn master_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_AVX512")
}
pub fn ifma_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512ifma")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Eight independent products, split at bit 52. Inputs must fit in 52 bits.
///
/// # Safety
/// Caller must check `ifma_available()` before calling.
#[cfg(target_arch = "x86_64")]
pub unsafe fn products52(a: &[u64; 8], b: &[u64; 8]) -> ([u64; 8], [u64; 8]) {
    let mut lo = [0u64; 8];
    let mut hi = [0u64; 8];
    // SAFETY: caller checks CPU/OS support. Arrays cover every unaligned
    // 64-byte access. Explicit-register asm keeps the workspace Rust 1.88 MSRV.
    unsafe {
        core::arch::asm!(
            "vmovdqu64 zmm0, [{a}]", "vmovdqu64 zmm1, [{b}]",
            "vpxord zmm2, zmm2, zmm2", "vpxord zmm3, zmm3, zmm3",
            "vpmadd52luq zmm2, zmm0, zmm1", "vpmadd52huq zmm3, zmm0, zmm1",
            "vmovdqu64 [{lo}], zmm2", "vmovdqu64 [{hi}], zmm3",
            "vpxord zmm0, zmm0, zmm0", "vpxord zmm1, zmm1, zmm1",
            "vpxord zmm2, zmm2, zmm2", "vpxord zmm3, zmm3, zmm3",
            a=in(reg) a.as_ptr(), b=in(reg) b.as_ptr(), lo=in(reg) lo.as_mut_ptr(), hi=in(reg) hi.as_mut_ptr(),
            out("zmm0") _, out("zmm1") _, out("zmm2") _, out("zmm3") _, options(nostack, preserves_flags)
        );
    }
    (lo, hi)
}

/// Exact full product of little-endian 64-bit limbs, using radix-2^52 IFMA rows.
/// Returns false without writing when the flag/CPU does not admit this path.
pub fn try_mul(a: &[u64], b: &[u64], out: &mut [u64]) -> bool {
    assert_eq!(out.len(), a.len() + b.len());
    if !rsa_enabled() {
        return false;
    }
    let _scrub = crate::scrub::ScrubOnDrop;
    #[cfg(target_arch = "x86_64")]
    {
        const MASK: u64 = (1 << 52) - 1;
        fn digits(a: &[u64]) -> Vec<u64> {
            (0..(a.len() * 64).div_ceil(52))
                .map(|i| {
                    let bit = i * 52;
                    let word = bit / 64;
                    let shift = bit % 64;
                    let mut v = a[word] >> shift;
                    if shift != 0 && word + 1 < a.len() {
                        v |= a[word + 1] << (64 - shift);
                    }
                    v & MASK
                })
                .collect()
        }
        let ad = digits(a);
        let bd = digits(b);
        let mut t = vec![0u64; ad.len() + bd.len()];
        for (i, &ai) in ad.iter().enumerate() {
            let mut carry = 0;
            for (batch, chunk) in bd.chunks(8).enumerate() {
                let mut bs = [0; 8];
                bs[..chunk.len()].copy_from_slice(chunk);
                // SAFETY: admission checked AVX-512F/IFMA; all digits are masked.
                let (lo, hi) = unsafe { products52(&[ai; 8], &bs) };
                for j in 0..chunk.len() {
                    let k = i + batch * 8 + j;
                    let v = t[k] + lo[j] + carry;
                    t[k] = v & MASK;
                    carry = hi[j] + (v >> 52);
                }
            }
            if !bd.is_empty() {
                t[i + bd.len()] = carry;
            }
        }
        out.fill(0);
        let mut acc = 0u128;
        let mut bits = 0;
        let mut dest = 0;
        for digit in t {
            acc |= (digit as u128) << bits;
            bits += 52;
            if bits >= 64 {
                if dest < out.len() {
                    out[dest] = acc as u64;
                }
                dest += 1;
                acc >>= 64;
                bits -= 64;
            }
        }
        if bits != 0 && dest < out.len() {
            out[dest] = acc as u64;
        }
        return true;
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ifma_full_products_match_u128_carry_oracle() {
        if !ifma_available() {
            return;
        }
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_AVX512_RSA", Some("1")),
                ("CRATONVM_AVX512", Some("1")),
            ],
            || {
                let mut seed = 0x123456789abcdefu64;
                for n in [0, 1, 2, 3, 4, 5, 7, 8, 9, 16, 32, 48, 64, 65, 128] {
                    for m in [0, 1, 3, 8, 17, 32, 64] {
                        for kind in 0..3 {
                            let mut make = |len| {
                                (0..len)
                                    .map(|_| {
                                        seed ^= seed << 13;
                                        seed ^= seed >> 7;
                                        seed ^= seed << 17;
                                        match kind {
                                            0 => u64::MAX,
                                            1 => 0,
                                            _ => seed,
                                        }
                                    })
                                    .collect::<Vec<u64>>()
                            };
                            let a = make(n);
                            let b = make(m);
                            let mut want = vec![0; n + m];
                            for (i, &ai) in a.iter().enumerate() {
                                let mut c = 0u128;
                                for (j, &bj) in b.iter().enumerate() {
                                    let v = ai as u128 * bj as u128 + want[i + j] as u128 + c;
                                    want[i + j] = v as u64;
                                    c = v >> 64;
                                }
                                if m != 0 {
                                    want[i + m] = c as u64;
                                }
                            }
                            let mut got = vec![0; n + m];
                            assert!(try_mul(&a, &b, &mut got));
                            assert_eq!(got, want, "n={n} m={m} kind={kind}");
                        }
                    }
                }
            },
        );
    }
    #[test]
    fn flags_can_disable_each_kernel_and_master_can_veto() {
        for (key, reader, default_on) in [
            ("CRATONVM_JIT_AVX512_RSA", rsa_enabled as fn() -> bool, true),
            ("CRATONVM_JIT_AVX512_EC", ec_enabled as fn() -> bool, true),
            ("CRATONVM_JIT_AVX512_AES", aes_enabled as fn() -> bool, true),
            ("CRATONVM_JIT_AVX512_PQC", pqc_enabled as fn() -> bool, true),
        ] {
            let explicit = cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT", None),
                    ("CRATONVM_AVX512", Some("1")),
                    (key, Some("1")),
                ],
                reader,
            );
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT", None),
                    ("CRATONVM_AVX512", None),
                    (key, None),
                ],
                || assert_eq!(reader(), if default_on { explicit } else { false }),
            );
            // Clear the grouped environment too: it is expanded after legacy
            // values and would otherwise re-enable the key being overridden.
            for value in [Some("0"), Some("off")] {
                cratonvm_types::flags::with_thread_overrides(
                    &[("CRATONVM_JIT", None), (key, value)],
                    || assert!(!reader()),
                );
            }
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT", None),
                    (key, Some("1")),
                    ("CRATONVM_AVX512", Some("0")),
                ],
                || assert!(!reader()),
            );
        }
    }
}
