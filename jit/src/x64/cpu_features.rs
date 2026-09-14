// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cached x86-64 CPU/OS feature detection for intrinsic selection.

use std::sync::atomic::{AtomicU8, Ordering};

static AVX2_SUPPORT: AtomicU8 = AtomicU8::new(0);
static SSE41_SUPPORT: AtomicU8 = AtomicU8::new(0);
static POPCNT_SUPPORT: AtomicU8 = AtomicU8::new(0);
static SSE42_SUPPORT: AtomicU8 = AtomicU8::new(0);
static LZCNT_SUPPORT: AtomicU8 = AtomicU8::new(0);
static BMI1_SUPPORT: AtomicU8 = AtomicU8::new(0);
static PCLMUL_SUPPORT: AtomicU8 = AtomicU8::new(0);

pub fn has_avx2() -> bool {
    cached_feature(&AVX2_SUPPORT, detect_avx2)
}

#[cfg(target_arch = "x86_64")]
fn detect_avx2() -> bool {
    // SAFETY: these read-only x86_64 intrinsics query CPU/OS feature state;
    // the architecture cfg guarantees that the instructions exist.
    unsafe {
        let leaf1 = std::arch::x86_64::__cpuid(1);
        if leaf1.ecx & (1 << 27) == 0 {
            return false;
        }
        if std::arch::x86_64::_xgetbv(0) & 0x06 != 0x06 {
            return false;
        }
        std::arch::x86_64::__cpuid_count(7, 0).ebx & (1 << 5) != 0
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn detect_avx2() -> bool {
    false
}

pub fn has_sse41() -> bool {
    cached_feature(&SSE41_SUPPORT, || cpuid_bit(1, 0, 2, 19))
}

#[cfg(target_arch = "x86_64")]
fn cpuid_bit(leaf: u32, sub: u32, reg: u8, bit: u32) -> bool {
    // SAFETY: CPUID is a read-only feature query and this branch is compiled
    // only for x86_64.
    #[allow(unused_unsafe)]
    unsafe {
        let result = std::arch::x86_64::__cpuid_count(leaf, sub);
        let value = match reg {
            0 => result.eax,
            1 => result.ebx,
            2 => result.ecx,
            _ => result.edx,
        };
        value & (1 << bit) != 0
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn cpuid_bit(_leaf: u32, _sub: u32, _reg: u8, _bit: u32) -> bool {
    false
}

fn cached_feature(slot: &AtomicU8, detect: impl FnOnce() -> bool) -> bool {
    let cached = slot.load(Ordering::Relaxed);
    if cached != 0 {
        return cached == 1;
    }
    let result = detect();
    slot.store(if result { 1 } else { 2 }, Ordering::Relaxed);
    result
}

pub fn has_popcnt() -> bool {
    cached_feature(&POPCNT_SUPPORT, || cpuid_bit(1, 0, 2, 23))
}

pub fn has_sse42() -> bool {
    cached_feature(&SSE42_SUPPORT, || cpuid_bit(1, 0, 2, 20))
}

pub fn has_lzcnt() -> bool {
    cached_feature(&LZCNT_SUPPORT, || cpuid_bit(0x8000_0001, 0, 2, 5))
}

pub fn has_bmi1() -> bool {
    cached_feature(&BMI1_SUPPORT, || cpuid_bit(7, 0, 1, 3))
}

pub fn has_pclmulqdq() -> bool {
    cached_feature(&PCLMUL_SUPPORT, || cpuid_bit(1, 0, 2, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_queries_are_stable() {
        let first = (has_avx2(), has_sse41(), has_popcnt());
        assert_eq!(first, (has_avx2(), has_sse41(), has_popcnt()));
    }
}
