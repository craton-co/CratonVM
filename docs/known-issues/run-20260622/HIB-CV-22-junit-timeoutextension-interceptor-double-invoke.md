# HIB-CV-22 — JUnit `TimeoutExtension` causes "InvocationInterceptors called invocation multiple times" (non-JIT)

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** Medium — real CratonVM-only correctness bug; **reproduces under `--nojit`** (not the JIT bug); intermittent
**Status:** **ROOT-CAUSED & RE-ATTRIBUTED (2026-06-23).** This is **not** a
thread/interrupt/timeout race — that hypothesis is **disproven** (the 120 s timeout
never fires for these millisecond-scale tests, so `InterruptTask`/`thread.interrupt()`
never run). It is the **HIB-CV-33 GC corruptor** (non-moving young-gen sweep under
`promotion_oom_risk`, `--nojit`) hitting JUnit's freshly-allocated
`ValidatingInvocation.invokedOrSkipped` `AtomicBoolean`, so
`compareAndSet(false,true)` spuriously returns `false`.
**See the full analysis + evidence:**
[`docs/internal/h2-suite-bugs/run-20260622/HIB-CV-22-junit-timeoutextension-double-invoke-is-gc-corruption.md`](../../internal/h2-suite-bugs/run-20260622/HIB-CV-22-junit-timeoutextension-double-invoke-is-gc-corruption.md).
Fix = same as [HIB-CV-33](../HIB-CV-33-sigsegv-execute-fault-joined-inheritance-sf-build.md)
(GC owner). The sections below are the **original (incorrect)** triage, kept for history.

---

## Symptom

Some `@Test` methods fail with:

```
org.junit.platform.commons.JUnitException: Chain of InvocationInterceptors called
invocation multiple times instead of just once: org.junit.jupiter.engine.extension.TimeoutExtension
  at org.junit.jupiter.engine.execution.InvocationInterceptorChain$ValidatingInvocation.fail(...)
  at org.junit.jupiter.engine.execution.InvocationInterceptorChain$ValidatingInvocation.proceed(...)
  at org.junit.jupiter.engine.extension.TimeoutExtension.intercept(TimeoutExtension.java:163)
  at org.junit.jupiter.engine.extension.TimeoutExtension.interceptTestMethod(TimeoutExtension.java:86)
... then: JUnitException: Failed to close extension context
```

Observed (standalone, `--nojit`):
- `org.hibernate.orm.test.annotations.onetoone.OneToOneJoinTableUniquenessTest` — **4 ok / 1 failed** (HotSpot: PASS all)
- also seen in the suite on `ManyToOneJoinTest` (but that class passed 3/3 on a standalone re-run → **intermittent**)

Both classes are **PASS on HotSpot**.

## Why it's a real CratonVM bug (not the JIT)

- Reproduces with `--nojit`, so it is independent of the JIT miscompile family
  (HIB-CV-20/21).
- HotSpot never hits it.

## Root area

The failing interceptor is JUnit's **`TimeoutExtension`** — active because the
suite sets `-Djunit.jupiter.execution.timeout.default=120s`. `TimeoutExtension`
runs the test body under a timeout (separate executor thread + interrupt) and the
`InvocationInterceptorChain$ValidatingInvocation` guard fires because the wrapped
invocation's `proceed()` is entered **more than once**.

This points at a CratonVM **thread / interrupt / timeout-execution concurrency
bug**: under the timeout extension's threaded execution, the test invocation is
driven twice (or the timeout path races with normal completion). The
intermittency (same class passes on some runs) is consistent with a timing race.

It may be *triggered* more often when a test runs slowly and approaches the 120 s
timeout (CratonVM is much slower than HotSpot here — see HIB-CV-21), but the
double-`proceed()` itself is a CratonVM defect: a correct VM never invokes the
chain twice regardless of timing.

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-OneToOneJoinTableUniquenessTest> 0
# intermittent: 1 of 5 tests fails with the InvocationInterceptors error.
```

## Suggested next step for a fixer

Reproduce with a minimal JUnit5 test that sets a short
`@Timeout`/`junit.jupiter.execution.timeout.default` and a deliberately slow test
body, then inspect CratonVM's handling of `TimeoutExtension`'s
`assertTimeoutPreemptively`-style execution (executor thread + `Future` +
`Thread.interrupt`). Likely a race between the timeout watchdog thread and the
main invocation completing, causing the interceptor `proceed()` to run twice.

## Triage

Real but **lower priority than the JIT** (HIB-CV-20/21), and partly masked/triggered
by the slowness. Candidate to fix after the JIT, or hand off to whoever owns
thread/interrupt semantics.
