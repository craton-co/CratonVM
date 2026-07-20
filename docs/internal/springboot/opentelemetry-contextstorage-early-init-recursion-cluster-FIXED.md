# OpenTelemetry `ContextStorage`-early-init / event-publishing tests hit runaway interpreter recursion (`StackOverflowError`, or a genuine non-terminating growing call stack)

**Status: FIXED (confirmed already resolved on dev) 2026-07-20**

## Resolution

No new source change was needed this session. Both classes were re-run on current `dev` (`0e743ad6e`, worktree `fix/otel-contextstorage-recursion-20260720`) with a fresh `--release` CratonVM build:

- `OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests`: 3/3 repeat runs PASS in ~3.6-9s (both JIT-on default and `--nojit`), no hang, no growing stack.
- `OpenTelemetryTracingAutoConfigurationTests`: the previously-`StackOverflowError` method `shouldPublishEventsWhenContextStorageIsInitializedEarly` PASSES in isolation (JIT-on and `--nojit`); the full 36-test class PASSES 36/36 in ~71s (a single-method run that hit an unrelated in-progress `AnnotationsScanner.isWithoutHierarchy` recursive stack frame at a 45s snapshot was a false alarm — it is bounded, legitimate meta-annotation-hierarchy recursion, not a hang; the class completes fine given the ~70s it actually needs).

The most plausible fix is `b7309a005` / merge `1868ecc25` ("fix JUnit invoke adapter livelock", 2026-07-18, see [`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md)): it fixed `try_stackless_invoke`'s foreign-function downcall adapter fast path incorrectly probing field 0 on any zero-field receiver whose invoked method was named `invoke`/`invokeExact`/`invokeBasic` — which is exactly the method name JUnit's `InterceptingExecutableInvoker.invoke` uses, and exactly the class named in both this doc's captured stack shapes. That fix landed the day after this doc was filed (2026-07-17), and this doc's cluster was never re-verified against it until now. Not independently re-confirmed via a fresh bisect against a pre-`b7309a005` binary (the isolated-worktree workflow builds only current `dev`), so treat the causal link as high-confidence but not a proven bisection.

---

**Original filing below, for history.**

**Status: OPEN — found 2026-07-17**, while re-verifying
[`contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md`](contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md).

## Symptom

Two classes in `module/spring-boot-micrometer-tracing-opentelemetry`, both
touching OpenTelemetry's `ContextStorage` early-initialization / event
publishing mechanism, show a recursion-shaped failure:

| Class | Method | Symptom |
|---|---|---|
| `OpenTelemetryTracingAutoConfigurationTests` | `shouldPublishEventsWhenContextStorageIsInitializedEarly` | `java.lang.StackOverflowError` (1/36 tests failed; class otherwise passes 35/36 given a long enough timeout — see cross-reference below) |
| `OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests` | (hangs immediately — every test method affected) | Does NOT terminate within a 25s `--stack-dump-on-timeout` watchdog; a captured interpreter stack dump showed the **main thread 3132 frames deep and still growing** in a tight cycle through `org.junit.jupiter.engine.execution.InterceptingExecutableInvoker.invoke`/`invokeVoid`/`lambda$invoke$0` → `InterceptingExecutableInvoker$ReflectiveInterceptorCall.lambda$ofVoidMethod$0` (repeating), i.e. genuine unbounded recursion through JUnit's own interceptor-invocation chain, not a parked/blocked thread |

Both classes construct `Mockito.mock(ContextStorage.class)` as an instance
field initializer
(`private final ContextStorage parent = Mockito.mock(ContextStorage.class);`)
and exercise `OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests`-style
publishing/wrapping of the mocked `ContextStorage`.

## How this was found

Found while re-verifying
[`contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md`](contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md) —
that doc originally lumped
`OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests`
in with 5 other classes as a "leaked-resource-then-silent-deadlock"
candidate, but explicitly flagged it as the weakest-confidence member (it
hangs almost immediately, not after many successful resource cycles like the
other 5). A live `--stack-dump-on-timeout` capture this session confirmed
that hedge: the thread is not parked at all — it is actively, repeatedly
re-entering the same interceptor-chain call shape at ever-increasing depth,
i.e. real unbounded recursion, an entirely different mechanism from the other
5 classes (which turned out to just be slow — see the FIXED doc).

`OpenTelemetryTracingAutoConfigurationTests`'s single residual failure
(`shouldPublishEventsWhenContextStorageIsInitializedEarly`, a
`StackOverflowError` rather than a hang) was found in the same investigation
pass and is grouped here on the strength of the shared "ContextStorage
early-init" test-method naming and shared module — not confirmed via a full
captured stack trace (JUnit's console summary format truncates
`StackOverflowError` to the exception type with no frames in this harness).

## Root cause — NOT confirmed at the source level

Not root-caused. The captured evidence for the wrapper-beans class
(1500+ repeating `InterceptingExecutableInvoker` frames, still growing) is
structurally similar to the recursive-self-invocation mechanism documented
in
[`modifiedclasspath-aether-network-hang-cluster.md`](modifiedclasspath-aether-network-hang-cluster.md)
(a `ModifiedClassPathExtension` guard that never trips, so each JUnit
invocation re-launches a nested `Launcher` forever) — **but neither affected
class here uses `@ClassPathExclusions`/`@ClassPathOverrides`/`@ForkedClassPath`**
(confirmed via source read), so it cannot be the same `ModifiedClassPathExtension`
guard. If there is a shared mechanism, it would have to be a different
interceptor-chain re-entrancy guard elsewhere in CratonVM's classloading/
reflection dispatch that similarly fails to recognize "this is the second
pass" — not identified this session.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-micrometer-tracing-opentelemetry` | `org.springframework.boot.micrometer.tracing.opentelemetry.autoconfigure.OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests` (hangs on every test method — confirmed genuine unbounded recursion via stack dump) |
| `module/spring-boot-micrometer-tracing-opentelemetry` | `org.springframework.boot.micrometer.tracing.opentelemetry.autoconfigure.OpenTelemetryTracingAutoConfigurationTests` (1/36 — `shouldPublishEventsWhenContextStorageIsInitializedEarly`, weaker-confidence grouping) |
