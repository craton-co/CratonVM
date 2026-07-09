# `RetryTemplateTests` Mockito mock-invocation throughput - FIXED

Status: fixed on dev (2026-07-09)

Original note: `docs/known-issues/retrytemplatetests-jit-mock-invocation-throughput.md`

## Summary

`org.springframework.core.retry.RetryTemplateTests$TimeoutTests` used a
20 ms timeout budget around retry bookkeeping. Under CratonVM, the first
Mockito `RetryListener` mock invocation could consume roughly 50-100 ms, so the
retry timeout check fired after the first failure and returned `Boom 1` where
the test expected the third invocation (`Boom 3`).

The current dev baseline had already narrowed the original three failing
timeout methods to one remaining failure:

- `retryableWithTimeoutExceededAfterSecondRetry`

Focused baseline result:

```text
RESULT org.springframework.core.retry.RetryTemplateTests$TimeoutTests found=6 succ=5 fail=1
```

## Root Cause

The slow path was not JIT code generation or GC. It was repeated third-party
Mockito/Byte Buddy runtime work on the first listener invocation for each fresh
test instance:

1. `MockMethodAdvice.isOverridden(Object, Method)` compiled a Byte Buddy
   `MethodGraph` for the generated `$MockitoMock$` listener class before the
   first interface-method invocation.
2. `LocationFactory.create()` created a Mockito diagnostic `Location`, which
   used `StackWalker` on every mock invocation.

Both paths were expensive enough to break a 20 ms framework timeout, even
though they are not semantically meaningful for this generated interface mock
case.

## Fix

The fix adds force-dispatched native intrinsics for the hot Mockito/Byte Buddy
methods:

- Byte Buddy `MethodGraph$Compiler$Default$Key.hashCode/equals`
- Mockito `LocationFactory.create()` and
  `LocationFactory$DefaultLocationFactory.create(boolean)`
- Mockito `MockMethodAdvice.isOverridden(Object, Method)`

For generated `$MockitoMock$` receivers, the `isOverridden` native preserves
normal Mockito interception by returning "not overridden" without compiling a
Byte Buddy `MethodGraph`. This is the correct answer for the Spring
`RetryListener` interface mock path and avoids spending the timeout budget
before retry bookkeeping can continue.

## Validation

Unique remote binary:

```text
bin/cratonvm-retrytemplate-mockito-overridden-20260709-005
```

Focused Rust tests:

```text
cargo test -p cratonvm-native-builtins bytebuddy_method_list_intrinsics_are_registered_for_descriptors --lib
cargo test -p cratonvm-native-builtins mockito_debugging_intrinsics_are_registered_for_location_factory --lib
```

Both passed.

Timing probe after the fix:

```text
retryable 1 enter elapsedMs=0
listener onRetryableExecution elapsedMs=4
listener beforeRetry elapsedMs=5
retryable 2 enter elapsedMs=5
listener onRetryFailure elapsedMs=6
listener onRetryableExecution elapsedMs=6
listener beforeRetry elapsedMs=7
retryable 3 enter elapsedMs=7
listener onRetryFailure elapsedMs=108
listener onRetryableExecution elapsedMs=109
listener onRetryPolicyTimeout elapsedMs=109
caught elapsedMs=109 cause=Boom 3 invocations=3
```

Focused suite results:

```text
RESULT org.springframework.core.retry.RetryTemplateTests$TimeoutTests found=6 succ=6 fail=0 skip=0 abort=0 ms=1885 status=OK
RESULT org.springframework.core.retry.RetryTemplateTests found=24 succ=24 fail=0 skip=0 abort=0 ms=3947 status=OK
RESULT org.springframework.core.retry.RetryTemplateTests$TimeoutTests found=6 succ=6 fail=0 skip=0 abort=0 ms=2687 status=OK  # --nojit
```

The known issue is retired because the original timeout-family repro now passes
under both default JIT mode and `--nojit`.
