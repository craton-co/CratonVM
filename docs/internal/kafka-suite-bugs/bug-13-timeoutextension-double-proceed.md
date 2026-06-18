# Bug 13 — `JUnitException: Chain of InvocationInterceptors called invocation multiple times … TimeoutExtension`

## RESOLVED (2026-06-12) — fixed by the bug-08 future/scheduled-executor fixes

This was a downstream symptom of the broken `@Timeout` future machinery, not a
separate defect. After [bug-08](bug-08-completablefuture-synthetic-layout-real-subclass.md)
(CompletableFuture/ScheduledFuture layout + `complete(null)`/chaining) landed, the
`TimeoutExtension` invocation no longer re-enters, so the JUnit double-`proceed`
detector is not tripped.

**Verified:** `FenceProducersHandlerTest` **4/4** (was 4/4 FAIL); `ProducerBatchTest`
no longer emits the `Chain of InvocationInterceptors` error (its remaining 2 fails
are [bug-15](bug-15-decompress-zlib-gzip.md) decompression); `TimeoutRepro` passes.
No additional VM change was needed beyond bug-08.

---


**Severity:** Medium — 5 failures; breaks all 4 of `FenceProducersHandlerTest`
and tests in `producer.internals.ProducerBatchTest`. Reproduces under `--nojit`.
HotSpot clean.

## Symptom
```
=> org.junit.platform.commons.JUnitException: Chain of InvocationInterceptors
   called invocation multiple times instead of just once:
   org.junit.jupiter.engine.extension.TimeoutExtension
```
Every `@Timeout`-annotated test in the affected class fails with this *framework*
error (not a test-body assertion).

## Context / relationship to bug-07
This surfaced **after** [bug-07](bug-07-timeout-scheduledfuture-cancel-ame.md) was
fixed: previously these `@Timeout` tests died earlier at `Future.cancel` AME, so
this was masked. JUnit's `InvocationInterceptorChain` asserts each
`InvocationInterceptor` calls `invocation.proceed()` **exactly once**;
`TimeoutExtension`'s `SameThreadTimeoutInvocation.proceed()` calls the delegate
once and `future.cancel(false)` once. On CratonVM the chain observes the delegate
invocation `proceed()` running **more than once**.

**To verify:** whether bug-07's scheduled-executor changes (the `schedule`/`submit`
natives or `ScheduledFuture.cancel` semantics) cause the timeout invocation to
re-enter `proceed()` — e.g. the timeout machinery double-dispatching the wrapped
invocation. Likely candidates:
- `native_stpe_submit_*` (separate-thread timeout mode) executing the submitted
  callable inline *and* the future being resolved a second time;
- a CratonVM dispatch path invoking the interceptor lambda twice.

A minimal repro is a single `@Timeout` test whose body increments a counter; assert
the body runs exactly once and no `JUnitException` is thrown.

## Affected classes (partial — append more later)
- admin.internals.FenceProducersHandlerTest (4/4 fail)
- producer.internals.ProducerBatchTest (some)
