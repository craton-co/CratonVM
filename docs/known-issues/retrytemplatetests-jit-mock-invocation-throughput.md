# `RetryTemplateTests` timing failures — Mockito mock invocation ~16x slower than HotSpot under JIT

Status: open

Date observed: 2026-07-03

## Summary

Three `org.springframework.core.retry.RetryTemplateTests$TimeoutTests` methods
fail under CratonVM real-JDK+JIT mode (HotSpot passes):

- `retryableWithTimeoutExceededAfterFirstDelayButBeforeFirstRetry`
- `retryableWithTimeoutExceededAfterFirstRetry`
- `retryableWithTimeoutExceededAfterSecondRetry`

All three configure a `RetryPolicy` with `timeout(Duration.ofMillis(20))` and
expect the **first** `checkIfTimeoutExceeded` check (`RetryTemplate.java:150`,
called immediately after the initial `retryable.execute()` throws, with
`sleepTime=0`) to *not* have exceeded the 20ms budget yet — i.e. the test
assumes the retry infrastructure's own bookkeeping (one `RetryListener` mock
invocation, one exception construct/throw/catch, a couple of `LogAccessor`
debug calls) completes in near-zero wall-clock time, as it does on HotSpot.

## Root cause (measured, not guessed)

A standalone timing probe (mock creation + one warm `RetryListener.onXxx()`
invocation + exception throw/catch, run via `KRun`-style direct execution
against the same test classpath) shows:

```
                          HotSpot      CratonVM (jit-real)
second (warm) mock invoke   0.8ms        11-31ms   (3 runs: 12.3, 25.2, 19.2ms)
simulated retry-prefix      0ms          11-24ms
```

A single **warm** Mockito mock method invocation costs roughly 15-25ms under
CratonVM's JIT vs <1ms on HotSpot — by itself close to or exceeding the
entire 20ms budget these three tests assume. This is not a bug in the retry
logic or in `System.currentTimeMillis()` (an earlier investigation attempt
guessed the latter without measuring; refuted — `1000x currentTimeMillis`
costs ~1.3ms total, i.e. ~1.3µs/call, matching HotSpot).

This throughput gap is a known, already-documented, deliberate architectural
trade-off: see the `bug-01-junit-reflection-heavy-jit-frame-scan-throughput`
history — precise JIT GC-oop-maps (default-on, needed for A2/A3/A4 GC-root
correctness) add a per-invocation frame-record + per-safepoint flush to every
JIT-compiled method, making call-heavy code (Mockito's ByteBuddy-generated
subclass → interceptor chain → matcher/answer lookup is many frames deep)
several times slower than the interpreter or HotSpot's JIT. That prior
investigation explicitly states this codegen cost is **not safe to disable
by default** (it's GC-root-coverage load bearing) and is **owned by a
separate, ongoing JIT-throughput workstream** — not something to change as
part of an unrelated bug-fix pass.

## Repro

```bash
cd apps/spring-suite-runner
export PATH=/usr/bin:/bin:$PATH
CRATONVM_BIN=$PWD/vmfrozen/cratonvm-rerun.exe KRUN_STACK=1 \
  ./run-suite.sh run --jdk real --jit on --batch 1 --only 'core\.retry\.RetryTemplateTests'
```

## Why not fixed here

The only real fix is improving JIT call-heavy throughput generally (the JIT
codegen workstream referenced above), which is explicitly out of scope for a
correctness-focused `core.*` bug-cluster pass and risks the GC-root-coverage
fixes that codegen currently provides. These three tests are also inherently
timing-fragile (a 20ms budget around framework-mock-invocation + exception
handling) even on a correct-but-slower JVM implementation.

## Suggested next steps

Track alongside the JIT-throughput workstream
(`docs/internal/app-jvm-bugs/bug-01-junit-reflection-heavy-jit-frame-scan-throughput.md`
and `precise-maps-inline-frame-record` follow-ups). Once per-call JIT
overhead for deep/megamorphic call chains (ByteBuddy proxy dispatch
specifically) comes down, re-run this class before concluding it needs a
test-side fix.
