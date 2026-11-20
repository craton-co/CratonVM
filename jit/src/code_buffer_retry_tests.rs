use super::{
    code_buffer_hint, code_buffer_retries_exhausted, note_code_buffer_shortfall,
    ExecutableBuffer, CODE_BUFFER_TOO_SMALL_SITE, MAX_CODE_BUFFER_RETRIES,
};

#[test]
fn osr_capacity_bails_retry_until_the_same_resize_budget_is_exhausted() {
    let registry = super::JitVerdictRegistry::default();
    let cid = cratonvm_types::ClassId::new(91_234);
    let class = "test/OsrCapacityRetry";
    let key = "test/OsrCapacityRetry.loop:()V";
    let mut capacity = 4096;
    for step in 1..=MAX_CODE_BUFFER_RETRIES {
        note_code_buffer_shortfall(key, capacity + 100, capacity);
        super::note_jit_bail_site(CODE_BUFFER_TOO_SMALL_SITE);
        registry.mark_bail_listed_with_site(cid, class, "loop", "()V");
        assert_eq!(
            registry.is_bail_listed(cid, class, "loop", "()V"),
            step == MAX_CODE_BUFFER_RETRIES,
            "OSR and method-entry compilation must agree on a resize retry",
        );
        assert_eq!(
            registry
                .bail_reason_for(cid, class, "loop", "()V")
                .as_deref(),
            Some(CODE_BUFFER_TOO_SMALL_SITE),
        );
        capacity = code_buffer_hint(key).unwrap();
    }
    // A real codegen invariant is permanent even with a measured size
    // hint. The capacity exception must not mask a different failure.
    let other = "test/OsrCapacityInvariant";
    let other_cid = cratonvm_types::ClassId::new(91_235);
    assert!(!registry.is_bail_listed(other_cid, other, "loop", "()V"));
    note_code_buffer_shortfall("test/OsrCapacityInvariant.loop:()V", 5000, 4096);
    super::note_jit_bail_site("short-branch-unencodable");
    registry.mark_bail_listed_with_site(other_cid, other, "loop", "()V");
    assert!(registry.is_bail_listed(other_cid, other, "loop", "()V"));
    assert!(super::take_jit_bail_site().is_none());
}

/// A method nobody ever measured has no hint, so the heuristic estimate
/// stands and codegen is byte-identical to before this machinery existed.
#[test]
fn an_unmeasured_method_has_no_hint() {
    assert_eq!(code_buffer_hint("com/example/Never.touched:()V"), None);
    // An empty key is what the legacy `compile()` test wrapper passes; it
    // must not collide with a real method under the empty string.
    note_code_buffer_shortfall("", 99_999, 99_999);
    assert_eq!(code_buffer_hint(""), None);
    assert!(!code_buffer_retries_exhausted(""));
}

/// The recorded shortfall comes back doubled — `wanted` under-reports,
/// because an out-of-bounds `try_patch_*` never adds to it.
#[test]
fn a_measured_shortfall_comes_back_doubled() {
    let key = "com/example/Big.method:(I)V";
    note_code_buffer_shortfall(key, 6238, 4096);
    assert_eq!(code_buffer_hint(key), Some(12_476));
}

/// Two attempts can take different paths through one method, so the
/// registry keeps the LARGEST measurement. Sizing the next buffer from a
/// later, smaller observation would overflow again — and that second
/// overflow would look identical to the first, which is how a "retry" that
/// never converges hides.
#[test]
fn a_smaller_later_measurement_does_not_shrink_the_hint() {
    let key = "com/example/Bimodal.method:()V";
    note_code_buffer_shortfall(key, 15_519, 4096);
    note_code_buffer_shortfall(key, 4_000, 4096);
    assert_eq!(code_buffer_hint(key), Some(31_038));
}

/// A zero `wanted` is not a measurement and must not install a hint of 0,
/// which `max` would silently ignore but which would make the registry
/// claim the method had been measured.
#[test]
fn a_zero_measurement_is_not_recorded() {
    let key = "com/example/Zero.method:()V";
    note_code_buffer_shortfall(key, 0, 0);
    assert_eq!(code_buffer_hint(key), None);
}

/// THE NON-CONVERGENCE THIS FIXED. `wanted` can be recorded well BELOW the
/// capacity that failed (`try_patch_*` overruns add nothing to it), and the
/// driver sizes the next buffer as `max(heuristic, hint)`. With the hint
/// derived from `wanted` alone, a `wanted` under half the failing capacity
/// produced a hint the failing capacity already beat — so the "retry"
/// allocated the SAME number of bytes and failed in the same place, on every
/// warmup-gate re-attempt, forever. Folding the failed capacity into the
/// measurement makes each attempt strictly larger than the last.
#[test]
fn the_hint_always_exceeds_the_capacity_that_failed() {
    let key = "com/example/UnderReported.method:()V";
    let failed_capacity = 171_424;
    note_code_buffer_shortfall(key, 12_000, failed_capacity);
    let hint = code_buffer_hint(key).expect("a measured method has a hint");
    assert!(
        hint > failed_capacity,
        "hint {hint} must exceed the capacity {failed_capacity} that just failed, \
         or the retry re-runs the identical compile"
    );
}

/// The bail-list exemption is bounded. Each retry is a full re-lowering plus
/// an mmap/munmap on the calling thread; without a cap a method the estimate
/// can never satisfy pays that on every re-attempt for the whole run.
#[test]
fn concurrent_shortfalls_at_one_capacity_share_a_retry_step() {
    let key = "com/example/Concurrent.method:()V";
    note_code_buffer_shortfall(key, 53_245, 51_392);
    let first_hint = code_buffer_hint(key).expect("first overflow records a hint");
    for _ in 0..12 {
        // Hot workers can cross the threshold together and compile the same
        // method before any failure publishes the larger capacity.
        note_code_buffer_shortfall(key, 53_245, 51_392);
    }
    assert_eq!(code_buffer_hint(key), Some(first_hint));
    assert!(
        !code_buffer_retries_exhausted(key),
        "same-capacity failures must leave the resize retry available"
    );
    note_code_buffer_shortfall(key, 53_245, first_hint);
    assert!(
        !code_buffer_retries_exhausted(key),
        "the larger capacity is a second distinct resize step"
    );
}

#[test]
fn the_retry_exemption_runs_out() {
    let key = "com/example/Hopeless.method:()V";
    let mut capacity = 8192;
    for i in 0..MAX_CODE_BUFFER_RETRIES {
        assert!(
            !code_buffer_retries_exhausted(key),
            "capacity step {i} must still be allowed to retry"
        );
        note_code_buffer_shortfall(key, capacity, capacity);
        capacity *= 2;
    }
    assert!(
        code_buffer_retries_exhausted(key),
        "after {MAX_CODE_BUFFER_RETRIES} distinct failed capacities the refusal must become permanent"
    );
}

/// A codegen invariant that has no encoding is NOT a sizing problem, and it
/// must not be reported or retried as one: `mark_codegen_unencodable` still
/// discards the method (so every `if buf.overflowed()` bail keeps working)
/// but names its own reason, which the driver reports instead of the
/// code-buffer message and which `try_compile` bail-lists permanently.
#[test]
fn an_unencodable_codegen_invariant_is_not_reported_as_a_short_buffer() {
    let mut buf = ExecutableBuffer::new(4096).expect("allocate");
    assert!(!buf.overflowed());
    assert_eq!(buf.codegen_failure_reason(), None);
    buf.mark_codegen_unencodable("rel8-displacement-out-of-range");
    assert!(
        buf.overflowed(),
        "the existing overflowed() discards must still fire"
    );
    assert_eq!(
        buf.codegen_failure_reason(),
        Some("rel8-displacement-out-of-range")
    );

    // A genuine capacity overflow reports NO named reason, so the driver
    // keeps taking the measure-and-retry path for it.
    let mut small = ExecutableBuffer::new(4096).expect("allocate");
    small.emit(&[0u8; 8192]);
    assert!(small.overflowed());
    assert_eq!(small.codegen_failure_reason(), None);
    assert!(
        small.wanted() > small.capacity(),
        "a real overflow's `wanted` exceeds its capacity; that is what the retry measures"
    );
}

/// The FIRST reason wins. A codegen site that bails mid-instruction can
/// leave later emitters running against a sticky-overflowed buffer, and
/// whichever of them marks next must not overwrite the diagnosis.
#[test]
fn the_first_named_reason_is_the_one_reported() {
    let mut buf = ExecutableBuffer::new(4096).expect("allocate");
    buf.mark_codegen_unencodable("deopt-stub-without-saved-regs");
    buf.mark_codegen_unencodable("rel8-displacement-out-of-range");
    assert_eq!(
        buf.codegen_failure_reason(),
        Some("deopt-stub-without-saved-regs")
    );
}

/// The exemption in `try_compile` matches on this constant, and the
/// backend records it. If either side were a bare literal, rewording one
/// would re-arm the permanent bail-list for overflowing methods — silently,
/// since the only symptom is a method that stops being compiled.
#[test]
fn the_retryable_site_name_is_shared_not_duplicated() {
    assert_eq!(CODE_BUFFER_TOO_SMALL_SITE, "code-buffer-estimate-too-small");
    // The predicate `try_compile` actually evaluates, spelled the same way.
    let site: Option<(&'static str, u32, u32)> = Some((CODE_BUFFER_TOO_SMALL_SITE, 0, 0));
    assert!(matches!(site, Some((CODE_BUFFER_TOO_SMALL_SITE, _, _))));
    // …and a neighbouring backend bail is NOT exempted.
    let other: Option<(&'static str, u32, u32)> =
        Some(("branch-target-not-an-instruction-boundary", 0, 0));
    assert!(!matches!(other, Some((CODE_BUFFER_TOO_SMALL_SITE, _, _))));
}
