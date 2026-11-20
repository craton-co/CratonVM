use super::*;

#[test]
fn feature_queries_are_stable() {
    let first = (has_avx2(), has_sse41(), has_popcnt());
    assert_eq!(first, (has_avx2(), has_sse41(), has_popcnt()));
}

#[test]
fn avx512_feature_queries_are_stable() {
    let first = (has_avx512f(), has_avx512bw(), has_avx512vl(), has_avx512dq());
    assert_eq!(first, (has_avx512f(), has_avx512bw(), has_avx512vl(), has_avx512dq()));
}

#[test]
fn avx512_kill_switch_honors_override() {
    cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_AVX512", Some("0"))], || {
        assert!(!avx512_enabled(), "CRATONVM_AVX512=0 must disable avx512_enabled");
    });
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

/// FMA3 is claimed only with its CPUID bit AND the OS's AVX state
/// (OSXSAVE, AVX, and XCR0 saving XMM+YMM): the inline `VFMADD213SD`
/// the `Math.fma` arm emits on this answer is VEX-encoded and would #UD
/// without the OS half. A claim here is also the precondition the
/// executed stub test `jit/tests/r12_rt3_fma_inline.rs` keys on.
#[cfg(target_arch = "x86_64")]
#[test]
fn r12w8_fma_is_claimed_only_with_its_bit_and_the_os_avx_state() {
    if !has_fma() {
        return;
    }
    assert!(
        cpuid_bit(1, 0, 2, 12),
        "FMA claimed without CPUID.1:ECX[12]"
    );
    assert!(cpuid_bit(1, 0, 2, 27), "FMA claimed without OSXSAVE");
    assert!(cpuid_bit(1, 0, 2, 28), "FMA claimed without AVX");
    // SAFETY: OSXSAVE is set (asserted above), so XGETBV is executable.
    let xcr0 = unsafe { std::arch::x86_64::_xgetbv(0) };
    assert_eq!(
        xcr0 & 0x06,
        0x06,
        "FMA claimed while the OS does not save YMM state"
    );
    assert_eq!(has_fma(), has_fma(), "the answer is stable");
}
