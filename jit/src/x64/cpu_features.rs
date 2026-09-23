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
        // OSXSAVE (bit 27) AND AVX (bit 28). Intel's documented AVX2 check
        // (SDM vol. 1 §14.7.1) requires CPUID.1:ECX.AVX as well as
        // CPUID.7.0:EBX.AVX2: a hypervisor that masks AVX off while passing
        // leaf 7 through reports AVX2 on a guest that cannot execute a single
        // VEX.256 instruction, and the SIMD loops would #UD.
        if leaf1.ecx & (1 << 27) == 0 || leaf1.ecx & (1 << 28) == 0 {
            return false;
        }
        if std::arch::x86_64::_xgetbv(0) & 0x06 != 0x06 {
            return false;
        }
        // Leaf 7 is basic but not universal; an unimplemented leaf answers
        // with another leaf's data rather than faulting. See
        // `cpuid_leaf_supported`.
        if !cpuid_leaf_supported(7) {
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

/// Is `leaf` inside the range the CPU actually implements?
///
/// CPUID does **not** fault on an unimplemented leaf: it returns the data of
/// the highest leaf in the same range instead. So reading a feature bit out of
/// `0x8000_0001` on a part that implements no extended leaves reads a bit of
/// some other leaf's output, and a set bit there is a false positive.
///
/// For `LZCNT` (the only extended-leaf query here) a false positive does not
/// fault either — `LZCNT` without ABM decodes as `REP BSR`, i.e. plain `BSR`,
/// which returns the *index* of the highest set bit rather than the count of
/// leading zeros, and answers an undefined value for a zero operand. That is a
/// silently wrong result in compiled code, which is the worst failure shape
/// available, so the max-leaf query is cheap insurance.
///
/// Every part on this build's baseline (`-C target-feature=+sse4.2`, i.e.
/// Nehalem / Bulldozer and later) does implement `0x8000_0000`, and reports
/// the LZCNT bit correctly — including the pre-Haswell Intel parts that report
/// it CLEAR. This guard is therefore expected never to fire; it exists so that
/// "expected" is not the only thing standing between the emitter and a wrong
/// answer.
#[cfg(target_arch = "x86_64")]
fn cpuid_leaf_supported(leaf: u32) -> bool {
    // The max-leaf query for a range is that range's base: 0 for basic leaves,
    // 0x8000_0000 for extended ones.
    let base = if leaf >= 0x8000_0000 { 0x8000_0000 } else { 0 };
    // SAFETY: CPUID is a read-only feature query and this branch is compiled
    // only for x86_64.
    #[allow(unused_unsafe)]
    let max = unsafe { std::arch::x86_64::__cpuid(base).eax };
    // A part that does not implement the extended range answers with something
    // below its base (commonly the basic max-leaf), which this comparison
    // rejects.
    max >= leaf && max >= base
}

#[cfg(not(target_arch = "x86_64"))]
fn cpuid_leaf_supported(_leaf: u32) -> bool {
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

/// `LZCNT` (ABM), reported in `CPUID.80000001h:ECX[5]`.
///
/// Gated on [`cpuid_leaf_supported`] because an unimplemented CPUID leaf
/// returns another leaf's data rather than faulting — see there for why a
/// false positive here is a wrong *answer* and not a crash.
pub fn has_lzcnt() -> bool {
    cached_feature(&LZCNT_SUPPORT, || {
        cpuid_leaf_supported(0x8000_0001) && cpuid_bit(0x8000_0001, 0, 2, 5)
    })
}

/// `BMI1` (`ANDN`/`BLSR`/`TZCNT`…), reported in `CPUID.07h:EBX[3]`.
///
/// Leaf 7 is a *basic* leaf and is absent on parts older than Ivy Bridge, so
/// the same max-leaf rule as [`has_lzcnt`] applies: without the guard,
/// `CPUID.07h` on such a part returns the highest basic leaf's data.
pub fn has_bmi1() -> bool {
    cached_feature(&BMI1_SUPPORT, || {
        cpuid_leaf_supported(7) && cpuid_bit(7, 0, 1, 3)
    })
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

    /// The max-leaf guard admits the leaves this host actually implements and
    /// refuses one no CPU implements.
    ///
    /// Not a tautology: the second half is what would have caught the missing
    /// guard. Without it, `cpuid_bit(0x8000_0001, ..)` on a part with no
    /// extended leaves reads another leaf's ECX, and a set bit 5 there turns
    /// `LZCNT` on where the instruction decodes as `BSR` — a wrong answer, not
    /// a fault.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_cpuid_max_leaf_guard_bounds_both_ranges() {
        assert!(
            cpuid_leaf_supported(0),
            "every x86_64 part implements CPUID leaf 0"
        );
        assert!(
            cpuid_leaf_supported(0x8000_0000),
            "every x86_64 part implements the extended max-leaf query"
        );
        assert!(
            !cpuid_leaf_supported(0x7FFF_FFFF),
            "a basic leaf just below the extended base is implemented by nothing"
        );
        assert!(
            !cpuid_leaf_supported(0xFFFF_FFFF),
            "the highest extended leaf is implemented by nothing"
        );
    }

    /// A feature gated on a leaf this host does not implement answers `false`,
    /// and every gated query is consistent with its own leaf guard.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_gated_feature_never_outlives_its_leaf() {
        if !cpuid_leaf_supported(0x8000_0001) {
            assert!(!has_lzcnt(), "LZCNT claimed without its leaf");
        }
        if !cpuid_leaf_supported(7) {
            assert!(!has_bmi1(), "BMI1 claimed without leaf 7");
            assert!(!has_avx2(), "AVX2 claimed without leaf 7");
        }
    }
}
