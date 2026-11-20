use super::*;
use cratonvm_jit::tiered::compile_panic_is_contained;

// VM-less tests have thread-local diagnostics. Each test uses a distinct
// helper name and asserts
// "at least" on the counter rather than an exact delta.
//
// That handles COLLISION. It does not handle CAPACITY, which is what
// actually broke: `RECENT_CAPACITY` is 8, eight tests in this module panic
// a helper, and they run concurrently -- so under load the ring can wrap
// and evict a test's own entry between the `contain` that wrote it and the
// `jit_helper_recent_panics()` that reads it back. Observed once in a
// full-workspace run on 2026-09-16
// (`a_panicking_body_returns_the_sentinel_and_is_counted`), never in
// isolation. Distinct names cannot help; eight slots shared by eight
// writers is a capacity problem.
//
// Every test that panics a helper takes this lock -- WRITERS as well as
// readers. Guarding only the readers is not enough and was measured not to
// be: an unguarded test that panics still writes the ring, so it can evict
// a lock-holder's entry between that holder's write and its read. One
// failure in sixteen concurrent runs survived the readers-only version.
// Poison is ignored: a panicking test here is a real failure to report,
// not a reason to fail the others.
static RING: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_panicking_body_returns_the_sentinel_and_is_counted() {
    let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    const NAME: &str = "helper_guard_test::panics_record";
    let before = jit_helper_panic_count();
    let got = contain(NAME, OnPanic::Record, -7i64, || -> i64 {
        panic!("deliberate helper panic (test)")
    });
    assert_eq!(
        got, -7,
        "a contained panic must return the wrap site's sentinel"
    );
    assert!(
        jit_helper_panic_count() > before,
        "a contained panic must increment the counter"
    );
    assert!(
        jit_helper_recent_panics().contains(&NAME),
        "a contained panic must record the helper name"
    );
}

#[test]
fn a_body_that_returns_normally_is_untouched() {
    let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    const NAME: &str = "helper_guard_test::returns_normally";
    assert_eq!(contain(NAME, OnPanic::Record, i64::MIN, || 42i64), 42);
    assert_eq!(
        contain(NAME, OnPanic::Record, 0usize, || usize::MAX),
        usize::MAX
    );
    contain(NAME, OnPanic::Record, (), || ());
    assert!(
        !jit_helper_recent_panics().contains(&NAME),
        "a body that did not panic must not be recorded"
    );
}

/// The crash handler's panic hook skips `hs_err` only inside this scope,
/// so a body that stopped running inside it would write a false crash
/// report for every contained panic.
#[test]
fn the_body_runs_inside_the_scope_the_crash_handler_honours() {
    let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    const NAME: &str = "helper_guard_test::scope";
    assert!(!compile_panic_is_contained());
    let inside = contain(NAME, OnPanic::Record, false, compile_panic_is_contained);
    assert!(
        inside,
        "the guarded body must run inside the contained-panic scope"
    );
    assert!(!compile_panic_is_contained());
}

#[test]
fn only_the_first_panic_in_a_helper_is_reported() {
    let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    const NAME: &str = "helper_guard_test::dedup";
    assert!(
        with_current_panic_state(|state| record_helper_panic(state, NAME)),
        "the first panic in a helper owes a line"
    );
    assert!(
        !with_current_panic_state(|state| record_helper_panic(state, NAME)),
        "a repeat panic must not print again"
    );
    let recorded = jit_helper_recent_panics()
        .iter()
        .filter(|n| **n == NAME)
        .count();
    assert_eq!(recorded, 2, "both panics are still recorded");
}

#[test]
fn a_non_string_payload_is_still_contained() {
    let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    const NAME: &str = "helper_guard_test::any_payload";
    let got = contain(NAME, OnPanic::Record, 3u8, || -> u8 {
        std::panic::panic_any(1234u32)
    });
    assert_eq!(got, 3);
}

/// `Deopt` must raise the flag that turns an `i64::MIN` return into a
/// sentinel. The flag is thread-local, so this is deterministic.
#[test]
fn the_deopt_policy_raises_the_deopt_flag() {
    let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    const NAME: &str = "helper_guard_test::deopt";
    let _ = crate::jit::helpers::take_jit_deopt_pending();
    let got = contain(NAME, OnPanic::Deopt, i64::MIN, || -> i64 {
        panic!("deliberate leaf helper panic (test)")
    });
    assert_eq!(got, i64::MIN);
    assert!(
        crate::jit::helpers::take_jit_deopt_pending(),
        "a contained leaf panic must leave the deopt flag set"
    );
}

/// `Deopt` on a void helper (e.g. `jit_bastore`, `jit_putfield_*`) must also
/// raise the deopt flag so the interpreter drains and recovers.
#[test]
fn the_deopt_policy_works_for_void_helpers() {
    let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    const NAME: &str = "helper_guard_test::deopt_void";
    let _ = crate::jit::helpers::take_jit_deopt_pending();
    contain(NAME, OnPanic::Deopt, (), || {
        panic!("deliberate void helper panic (test)")
    });
    assert!(
        crate::jit::helpers::take_jit_deopt_pending(),
        "a contained void helper panic must leave the deopt flag set"
    );
}

/// The `Throw` policy with no JIT thread installed, which is every unit
/// test thread: the stash declines, and the sentinel is still returned. A
/// stash that actually builds the `InternalError` needs a booted VM and is
/// not covered here.
#[test]
fn the_throw_policy_degrades_to_the_sentinel_without_a_jit_thread() {
    let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    const NAME: &str = "helper_guard_test::throw_no_thread";
    assert!(!crate::jit::helpers::is_jit_thread_set());
    assert!(
        !crate::jit::helpers::stash_jit_helper_panic(0, NAME, "detail"),
        "no JIT thread means nothing can be stashed"
    );
    let got = contain(NAME, OnPanic::Throw { vm_ptr: 0 }, 0i64, || -> i64 {
        panic!("deliberate allocating helper panic (test)")
    });
    assert_eq!(got, 0);
    assert!(jit_helper_recent_panics().contains(&NAME));
}

/// The end-of-run surface, which is the whole point of RT-1's second half:
/// a process that took a contained panic must be able to SAY SO at exit,
/// rather than the fact living only in a stderr line thousands of lines
/// back and in a counter the crash handler alone reads.
///
/// Asserted on content, not on formatting: the count is a number the
/// summary must carry, and the panicking helper's own name must appear,
/// because "N panics happened" without "in which helper" is not actionable
/// and was the state this replaced. The exact wording is deliberately not
/// pinned — the nightly sensor greps for the `[jit-helper-panic]` tag and
/// nothing else.
#[test]
fn the_end_of_run_summary_names_the_helpers_that_panicked() {
    let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    const NAME: &str = "helper_guard_test::summary";
    let _ = contain(NAME, OnPanic::Record, 0i64, || -> i64 {
        panic!("deliberate helper panic (test)")
    });
    let summary = jit_helper_panic_summary()
        .expect("a process that contained a panic must produce a summary");
    assert!(
        summary.starts_with("[jit-helper-panic]"),
        "the summary must carry the tag the nightly sensor greps for: {summary}"
    );
    assert!(
        summary.contains(&jit_helper_panic_count().to_string()),
        "the summary must carry the count: {summary}"
    );
    assert!(
        summary.contains(NAME),
        "the summary must name the helper that panicked: {summary}"
    );
}

/// The counter is process-wide and monotone, so the zero arm cannot be
/// asserted in a process that also runs the tests above. What CAN be
/// asserted, and is the part that would actually break, is that the
/// summary is keyed on the counter rather than on the name log: a fresh
/// process has taken no panic, so it must produce no line at all — an
/// exit-time surface that printed "0 contained helper panics" on every
/// clean run would be noise nobody reads, which is how the previous
/// sensor failed.
#[test]
fn the_summary_is_absent_exactly_when_the_counter_is_zero() {
    let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        jit_helper_panic_summary().is_none(),
        jit_helper_panic_count() == 0,
        "the summary's presence must be exactly the counter's non-zero-ness"
    );    }