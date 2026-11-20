use super::{bytecode_callee_tier_up_threshold, method_wide_sync};

#[test]
fn synchronized_compiled_callee_uses_fast_boot_warmup_unless_overridden() {
    assert_eq!(bytecode_callee_tier_up_threshold(2_000, true, false, false), 500);
    assert_eq!(bytecode_callee_tier_up_threshold(500, true, false, false), 500);
    assert_eq!(bytecode_callee_tier_up_threshold(2_000, true, false, true), 2_000);
    assert_eq!(bytecode_callee_tier_up_threshold(2_000, false, false, false), 2_000);
    assert_eq!(bytecode_callee_tier_up_threshold(2_000, true, true, false), 2_000);
}

/// The short-loop page's shape: 300 interpreted calls plus credit put the
/// method at 320 when its caller compiles; the template site's own count
/// is 16. The site takes 320, and a later sync that reaches 500 is a
/// crossing even though no exact stride point was hit.
#[test]
fn j27_a_template_site_takes_the_method_count_and_reports_the_crossing() {
    assert_eq!(method_wide_sync(16, 320, 500), (320, false));
    assert_eq!(method_wide_sync(336, 512, 500), (512, true));
    // Never backwards, and no crossing without a move.
    assert_eq!(method_wide_sync(600, 520, 500), (600, false));
    assert_eq!(method_wide_sync(500, 500, 500), (500, false));
    // Already past the threshold: the retry strides own it.
    assert_eq!(method_wide_sync(520, 700, 500), (700, false));
}
