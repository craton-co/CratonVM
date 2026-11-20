use super::*;
use crate::test_utils::mock_ctx;

fn nano_time() -> i64 {
    let mut ctx = mock_ctx();
    match native_system_nano_time(&mut ctx, &[]) {
        Ok(Some(Value::Long(v))) => v,
        other => panic!("nanoTime must answer a long, got {other:?}"),
    }
}

fn high_res_counter() -> i64 {
    let mut ctx = mock_ctx();
    match native_perf_high_res_counter(&mut ctx, &[]) {
        Ok(Some(Value::Long(v))) => v,
        other => panic!("highResCounter must answer a long, got {other:?}"),
    }
}

/// The clock never reads zero, INCLUDING on its very first call.
///
/// This is the guard on `MONOTONIC_ORIGIN_NANOS`. Without the offset the
/// first reading in a process is the gap between `Instant::now()` and the
/// next statement, which on Windows is below the clock's resolution — so
/// it came back as exactly 0 about half the time, disarming every
/// `if (started == 0) started = System.nanoTime();` timer in Java.
///
/// The assertion deliberately does not depend on being the first caller:
/// `START` is a process-wide `OnceLock` that a sibling test may already
/// have initialised, and the whole point of a fixed origin is that the
/// answer is positive either way.
#[test]
fn nano_time_is_never_zero() {
    let t = nano_time();
    assert!(
        t >= MONOTONIC_ORIGIN_NANOS,
        "nanoTime must start at a non-zero origin, got {t}"
    );
}

/// `jdk.internal.perf.Perf.highResCounter` had the identical lazy-origin
/// defect and now carries the identical guarantee.
#[test]
fn high_res_counter_is_never_zero() {
    let t = high_res_counter();
    assert!(
        t >= MONOTONIC_ORIGIN_NANOS,
        "highResCounter must start at a non-zero origin, got {t}"
    );
}

/// A constant offset must not cost monotonicity.
#[test]
fn nano_time_is_monotonic() {
    let t1 = nano_time();
    let t2 = nano_time();
    assert!(t2 >= t1, "nanoTime went backwards: {t1} then {t2}");
}

#[test]
fn high_res_counter_is_monotonic() {
    let t1 = high_res_counter();
    let t2 = high_res_counter();
    assert!(t2 >= t1, "highResCounter went backwards: {t1} then {t2}");
}
