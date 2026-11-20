use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// 0ms must return essentially immediately (no actual sleep).
#[test]
fn t19_n2_thread_sleep0_zero_millis_returns_immediately() {
    let mut ctx = mock_ctx();
    let start = std::time::Instant::now();
    let r = native_thread_sleep0(&mut ctx, &[Value::Long(0)]);
    let elapsed = start.elapsed();
    assert!(r.is_ok(), "sleep0(0) should succeed, got {:?}", r);
    assert!(
        elapsed < std::time::Duration::from_millis(5),
        "sleep0(0) should return in < 5ms, took {:?}",
        elapsed
    );
}

/// 10ms must actually block for at least ~10ms (chunk size is 100ms,
/// so a 10ms request sleeps for the full 10ms in one partial chunk).
#[test]
fn t19_n2_thread_sleep0_10ms_actually_sleeps() {
    let mut ctx = mock_ctx();
    let start = std::time::Instant::now();
    let r = native_thread_sleep0(&mut ctx, &[Value::Long(10_000_000)]);
    let elapsed = start.elapsed();
    assert!(r.is_ok(), "sleep0(10) should succeed, got {:?}", r);
    // Must have slept at least the requested amount.
    assert!(
        elapsed >= std::time::Duration::from_millis(10),
        "sleep0(10) should sleep в‰Ґ 10ms, took {:?}",
        elapsed
    );
    // Must not have wildly overslept (generous upper bound for CI).
    assert!(
        elapsed <= std::time::Duration::from_millis(200),
        "sleep0(10) should return within ~200ms upper bound, took {:?}",
        elapsed
    );
}

/// Negative millis в†’ IllegalArgumentException (defensive native check).
#[test]
fn t19_n2_thread_sleep0_negative_throws_illegal_argument() {
    let mut ctx = mock_ctx();
    let r = native_thread_sleep0(&mut ctx, &[Value::Long(-1)]);
    assert!(r.is_err(), "negative millis must throw");
    match r {
        Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::IllegalArgumentException { message },
            ),
        )) => {
            assert!(
                message.contains("negative"),
                "expected 'negative' in message, got: {}",
                message
            );
        }
        other => panic!(
            "expected IllegalArgumentException for negative millis, got {:?}",
            other
        ),
    }
}

/// When the interrupt flag is pre-set, sleep0(0) throws immediately.
#[test]
fn t19_n2_thread_sleep0_zero_millis_with_interrupt_throws() {
    let ctx = mock_ctx();
    ctx.set_interrupted(true);
    let mut ctx = ctx;
    let r = native_thread_sleep0(&mut ctx, &[Value::Long(0)]);
    assert!(r.is_err(), "sleep0(0) with interrupt flag must throw");
    // And the flag must be CLEARED per JDK spec.
    assert!(
        !ctx.is_interrupted(false),
        "interrupt flag must be cleared after InterruptedException",
    );
}

/// Interpreter round i1 wave 24 (L7): HotSpot's `JVM_SleepNanos` checks
/// the interrupt before the duration, so a zero sleep on an interrupted
/// thread throws and clears the flag -- through every sleep native
/// (`Thread.sleep(J)V`, `sleepNanos0`, `sleep(JI)V`). Each used to return
/// normally with the flag still set (`L7W23Interrupts` row `pre sleep0`).
#[test]
fn a_zero_sleep_on_an_interrupted_thread_throws_through_every_sleep_native() {
    type SleepNative = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;
    let natives: [(&str, SleepNative, Vec<Value>); 3] = [
        ("sleep(J)V", native_thread_sleep, vec![Value::Long(0)]),
        (
            "sleepNanos0",
            native_thread_sleep_nanos,
            vec![Value::Long(0)],
        ),
        (
            "sleep(JI)V",
            native_thread_sleep_millis_nanos,
            vec![Value::Long(0), Value::Int(0)],
        ),
    ];
    for (name, native, args) in natives {
        let mut ctx = mock_ctx();
        ctx.set_interrupted(true);
        let r = native(&mut ctx, &args);
        assert!(
            r.is_err(),
            "{name}: a zero sleep on an interrupted thread must throw"
        );
        assert!(
            !ctx.is_interrupted(false),
            "{name}: the throw must clear the interrupt flag"
        );
        // And an uninterrupted zero sleep still returns at once.
        let mut ctx = mock_ctx();
        assert!(native(&mut ctx, &args).is_ok(), "{name}: sleep(0) returns");
    }
}

/// Wave 24 (L7): `sleepNanos0` sleeps in slices and polls the interrupt,
/// so an interrupted long sleep ends promptly (it used to be one
/// uninterruptible `std::thread::sleep` of the whole duration). The mock
/// context is `!Sync`, so no other thread can raise its flag mid-sleep;
/// this test pins the pre-check path and its bounded duration, and the
/// mid-sleep path is covered by the `L7W23Interrupts` / `L7W24SleepInterrupts`
/// probes.
/// Interpreter round i5 wave 104, lane L6 (`L6W104SleepNanosNegative`):
/// HotSpot 25's `JVM_SleepNanos` throws `IllegalArgumentException
/// ("nanosecond timeout value out of range")` for a negative time, before
/// it looks at the interrupt (the flag stays set).
#[test]
fn sleep_nanos0_of_a_negative_time_throws_before_the_interrupt_check() {
    let mut ctx = mock_ctx();
    ctx.set_interrupted(true);
    let r = native_thread_sleep_nanos(&mut ctx, &[Value::Long(-1)]);
    match r {
        Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::IllegalArgumentException { message },
            ),
        )) => assert_eq!(message, "nanosecond timeout value out of range"),
        other => panic!("expected IllegalArgumentException, got {other:?}"),
    }
    assert!(ctx.is_interrupted(false), "the interrupt flag must stay set");
}

#[test]
fn sleep_nanos0_of_an_interrupted_thread_does_not_sleep() {
    let mut ctx = mock_ctx();
    ctx.set_interrupted(true);
    let start = std::time::Instant::now();
    let r = native_thread_sleep_nanos(&mut ctx, &[Value::Long(5_000_000_000)]);
    assert!(r.is_err(), "an interrupted sleepNanos0 must throw");
    assert!(
        start.elapsed() < std::time::Duration::from_millis(500),
        "an interrupted 5 s sleepNanos0 must not sleep, took {:?}",
        start.elapsed()
    );
}

/// Interrupt delivered mid-sleep: set the flag on the context, then
/// run sleep0 for a longer-than-chunk duration and verify that it
/// returns within one chunk (в‰¤ 110ms) with InterruptedException, and
/// that the interrupt flag is cleared per JDK spec.
///
/// We pre-set the flag because `MockNativeContext` is `!Sync`; the
/// chunked poll at loop-top runs BEFORE the first `std::thread::sleep`,
/// so a pre-set flag exercises the same code path as a flag that
/// arrives during an earlier chunk.
#[test]
fn t19_n2_thread_sleep0_interrupt_during_sleep() {
    let ctx = mock_ctx();
    ctx.set_interrupted(true);
    let mut ctx = ctx;
    let start = std::time::Instant::now();
    // Request a 2-second sleep вЂ” if interrupt polling is broken, the
    // test will hang for ~2s (still fail, but visibly).
    let r = native_thread_sleep0(&mut ctx, &[Value::Long(2_000_000_000)]);
    let elapsed = start.elapsed();
    assert!(r.is_err(), "sleep0 with interrupt must throw");
    match r {
        Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::InterruptedException,
            ),
        )) => { /* the bare fallback */ }
        // `sleepNanos0`'s own body builds the real exception
        // (`InterruptedException: sleep interrupted`, as HotSpot's
        // `JVM_SleepNanos` throws).
        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(_)) => {}
        other => panic!("expected InterruptedException, got {:?}", other),
    }
    // Interrupt flag must be cleared per JDK spec.
    assert!(
        !ctx.is_interrupted(false),
        "interrupt flag must be cleared after InterruptedException",
    );
    // Poll happens at top of loop (before first chunk sleep), so the
    // flag is detected within well under one chunk (100ms). Allow
    // 110ms for CI jitter.
    assert!(
        elapsed <= std::time::Duration::from_millis(110),
        "interrupt should be detected within в‰¤110ms, took {:?}",
        elapsed
    );
}
