# `spring-boot-grpc-test`: `NoSuchMethodError: SpringExtension.isBeanOverride`

**Status: OPEN — found 2026-07-17**

**Update 2026-07-17 (bin3 rerun triage):** the byte-identical failure shape
(same `ParameterResolutionException` wrapping the same missing
`isBeanOverride(Parameter)` method, on the exact same `Environment`
parameter-injection path) also hits `module/spring-boot-micrometer-tracing-test`,
1 test each in 2 classes:

```
JUnit Jupiter:AutoConfigureTracingMissingIntegrationTests:customizerRunsAndDisablesExportWhenNoAnnotationPresent(Environment)
    => org.junit.jupiter.api.extension.ParameterResolutionException: Failed to resolve parameter [org.springframework.core.env.Environment environment] in method [void org.springframework.boot.micrometer.tracing.test.autoconfigure.AutoConfigureTracingMissingIntegrationTests.customizerRunsAndDisablesExportWhenNoAnnotationPresent(org.springframework.core.env.Environment)]: org/springframework/test/context/junit/jupiter/SpringExtension.isBeanOverride(Ljava/lang/reflect/Parameter;)Z

JUnit Jupiter:AutoConfigureTracingPresentIntegrationTests:customizerDoesNotSetExportDisabledPropertyWhenAnnotationPresent(Environment)
    => org.junit.jupiter.api.extension.ParameterResolutionException: Failed to resolve parameter [org.springframework.core.env.Environment environment] in method [void org.springframework.boot.micrometer.tracing.test.autoconfigure.AutoConfigureTracingPresentIntegrationTests.customizerDoesNotSetExportDisabledPropertyWhenAnnotationPresent(org.springframework.core.env.Environment)]: org/springframework/test/context/junit/jupiter/SpringExtension.isBeanOverride(Ljava/lang/reflect/Parameter;)Z
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-tracing-test.org.springframework.boot.micrometer.tracing.test.au-9201c48b5991.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-tracing-test.org.springframework.boot.micrometer.tracing.test.au-b6d7f2a9af5d.out.log`

Both classes fail 1/1 test — each class only has this one `@Test` method,
and both take an `Environment` parameter, which is exactly the shared
trigger shape across every affected class in this doc (a single-test class
whose only test method injects `Environment`). This cross-module
recurrence (gRPC test support vs. Micrometer tracing test support — two
unrelated Spring Boot modules, both `-test`-support artifacts) further
supports the "classpath/class-linkage bug" framing over a module-specific
one; not independently re-investigated at the source level this session
either. Not folded into a single "Affected classes" entry per module below
since the mechanism (not just the symptom) is still unconfirmed.

## Symptom

| Module | Class | Wall time |
|---|---|---:|
| `module/spring-boot-grpc-test` | `AutoConfigureTestGrpcTransportOverrideTests` | 2.4s (1/1 fail) |
| `module/spring-boot-grpc-test` | `AutoConfigureTestGrpcTransportTests` | 2.1s (1/1 fail) |
| `module/spring-boot-micrometer-tracing-test` | `AutoConfigureTracingMissingIntegrationTests` | 9.1s (1/1 fail) |
| `module/spring-boot-micrometer-tracing-test` | `AutoConfigureTracingPresentIntegrationTests` | (1/1 fail) |

Both classes' single test fails identically, before the test body ever
runs — JUnit5 can't even resolve the test method's `Environment` parameter:

```
JUnit Jupiter:AutoConfigureTestGrpcTransportOverrideTests:setsEnabledPropertiesToTrue(Environment)
  => org.junit.jupiter.api.extension.ParameterResolutionException: Failed to resolve parameter [org.springframework.core.env.Environment environment] in method [void ...setsEnabledPropertiesToTrue(org.springframework.core.env.Environment)]: org/springframework/test/context/junit/jupiter/SpringExtension.isBeanOverride(Ljava/lang/reflect/Parameter;)Z
     org.junit.jupiter.engine.execution.ParameterResolutionUtils.resolveParameter(ParameterResolutionUtils.java:178)
     ...
   Caused by: java.lang.NoSuchMethodError: org/springframework/test/context/junit/jupiter/SpringExtension.isBeanOverride(Ljava/lang/reflect/Parameter;)Z
     org.junit.jupiter.engine.execution.ParameterResolutionUtils.resolveParameter(ParameterResolutionUtils.java:155)
```

`AutoConfigureTestGrpcTransportTests:setsEnabledPropertiesToFalse(Environment)`
fails with the byte-identical `NoSuchMethodError`.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-grpc-test.org.springframework.boot.grpc.test.autoconfigure.AutoConfigureTes-a712cbaf3ff8.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-grpc-test.org.springframework.boot.grpc.test.autoconfigure.AutoConfigureTes-e360f0ce0c8b.out.log`

## Root cause

**CONFIRMED 2026-07-17 (bin3 rerun triage), superseding the "not confirmed"
hypothesis below.** This is not a classpath/linkage bug at all — it's a
CratonVM native override that has drifted out of sync with the real
library it was written to shadow.

`native-builtins/src/lib.rs:7799-7804` registers a full native
reimplementation of
`SpringExtension.resolveParameter(ParameterContext, ExtensionContext)Object`
(`native_spring_extension_resolve_parameter`, implementation starting
`native-builtins/src/lib.rs:7974`). At `native-builtins/src/lib.rs:8053-8062`
that native implementation unconditionally calls
`SpringExtension.isBeanOverride(Parameter)Z` via `invoke_special` — but
`javap -p` against the real `spring-test-7.0.7.jar`'s `SpringExtension.class`
confirms **no `isBeanOverride` method exists on that class at all** in the
version actually on this classpath, and disassembling the real
`resolveParameter()` bytecode shows it no longer does any inline
`ApplicationContext`-type check or bean-override branching — it delegates
straight to `ParameterResolutionDelegate.resolveDependency(...)`. (The real
`junit-jupiter-engine-6.0.3.jar`'s `ParameterResolutionUtils.class` was also
checked and never references `isBeanOverride`/`SpringExtension` — ruling out
any real JUnit-side call, confirming the call originates entirely from
CratonVM's own native override.)

This native fast path was added 2026-07-08
(`docs/internal/fixed-suite-bugs/test-context-constructor-param-annotation-offset.md`,
which explicitly describes adding "a native `SpringExtension.resolveParameter(...)`
equivalent that preserves Spring's constructor scoping and `@BeanOverride`
fallback...") to match Spring Test's `resolveParameter()` shape *at that
time*. Spring Framework has since refactored the real method and dropped
`isBeanOverride` — the native override was never updated to match, so it
now throws `NoSuchMethodError` for every parameter that isn't exactly
`ApplicationContext`-typed (which is why every affected class here has an
`Environment`-typed test parameter — that's the first type this native path
falls through to the `isBeanOverride` check for).

**Fix direction (not implemented — investigation only):** either update
`native_spring_extension_resolve_parameter` to match the current
`spring-test-7.0.7` `resolveParameter()`/`ParameterResolutionDelegate`
shape (dropping the `isBeanOverride` call entirely), or retire the native
override and let real `SpringExtension` bytecode run directly if it no
longer needs the workaround the 2026-07-08 fix was for.

Previously-filed "not confirmed" hypothesis retained below for record; it
speculated about a classpath/class-linkage mismatch rather than a drifted
native override, which the `javap` evidence above rules out:
- a JIT/interpreter method-resolution ("vtable") cache serving a stale
  method table for this class specific to this native `Parameter`-based
  overload (`isBeanOverride(Parameter)` vs. a differently-shaped overload
  that may exist in an older `spring-test` version this cache could be
  keyed against).

Not root-caused against CratonVM source this round — would need to dump
the actual resolved classpath entries/class bytes for `SpringExtension` at
the point of failure (e.g. via a debug hook or `-verbose:class`-equivalent
tracing) to distinguish these.

## Update 2026-07-17 (bin13 rerun triage) — 2 more classes, a non-`Environment` parameter type

`module/spring-boot-webmvc-test`'s `MockMvcSpringBootTestIntegrationTests`
and `MockMvcTesterSpringBootTestIntegrationTests` each fail 1 of their 6
tests (`shouldTestWithRestTestClient(RestTestClient)`) with the
byte-identical `NoSuchMethodError`:

```
JUnit Jupiter:MockMvcSpringBootTestIntegrationTests:shouldTestWithRestTestClient(RestTestClient)
    => org.junit.jupiter.api.extension.ParameterResolutionException: Failed to resolve parameter [org.springframework.test.web.servlet.client.RestTestClient restTestClient] in method [void ...shouldTestWithRestTestClient(org.springframework.test.web.servlet.client.RestTestClient)]: org/springframework/test/context/junit/jupiter/SpringExtension.isBeanOverride(Ljava/lang/reflect/Parameter;)Z
     Caused by: java.lang.NoSuchMethodError: org/springframework/test/context/junit/jupiter/SpringExtension.isBeanOverride(Ljava/lang/reflect/Parameter;)Z
       org.junit.jupiter.engine.execution.ParameterResolutionUtils.resolveParameter(ParameterResolutionUtils.java:155)
```

This confirms the confirmed root cause's framing ("throws `NoSuchMethodError`
for every parameter that isn't exactly `ApplicationContext`-typed") is not
narrowly an `Environment`-typed-parameter issue — `RestTestClient` hits the
identical fallthrough path, so any non-`ApplicationContext` parameter type
is affected. The other 5 tests in each of these 2 classes pass fine (they
don't take a parameter that reaches this native fast path's fallthrough).
Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc-test.org.springframework.boot.webmvc.test.autoconfigure.mockmvc.Mock-13242e234abc.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc-test.org.springframework.boot.webmvc.test.autoconfigure.mockmvc.Mock-dde51d6ee296.out.log`

## Update 2026-07-17 (bin7 rerun triage) — 2 more classes, `Environment` parameter again

`module/spring-boot-micrometer-metrics-test`'s
`AutoConfigureMetricsMissingIntegrationTests` and
`AutoConfigureMetricsPresentIntegrationTests` each fail their one `@Test`
method (`customizerRunsAndSetsExclusionPropertiesWhenNoAnnotationPresent(Environment)`
/ `customizerDoesNotSetExclusionPropertiesWhenAnnotationPresent(Environment)`)
with the byte-identical failure shape as the original `grpc-test`/
`micrometer-tracing-test` entries above — same `Environment`-typed
parameter, same `ParameterResolutionException` wrapping
`NoSuchMethodError: SpringExtension.isBeanOverride(Ljava/lang/reflect/Parameter;)Z`:

```
JUnit Jupiter:AutoConfigureMetricsMissingIntegrationTests:customizerRunsAndSetsExclusionPropertiesWhenNoAnnotationPresent(Environment)
    => org.junit.jupiter.api.extension.ParameterResolutionException: Failed to resolve parameter [org.springframework.core.env.Environment environment] in method [void org.springframework.boot.micrometer.metrics.test.autoconfigure.AutoConfigureMetricsMissingIntegrationTests.customizerRunsAndSetsExclusionPropertiesWhenNoAnnotationPresent(org.springframework.core.env.Environment)]: org/springframework/test/context/junit/jupiter/SpringExtension.isBeanOverride(Ljava/lang/reflect/Parameter;)Z
```

A fourth `-test`-support module hitting the exact same confirmed drifted
native override (`native_spring_extension_resolve_parameter`,
`native-builtins/src/lib.rs:7974`) — consistent with the confirmed root
cause above, not re-investigated further. Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-metrics-test.org.springframework.boot.micrometer.metrics.test.au-afd644b19322.out.log`,
`...au-05eb53d385df.out.log`.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-grpc-test` | `org.springframework.boot.grpc.test.autoconfigure.AutoConfigureTestGrpcTransportOverrideTests` |
| `module/spring-boot-grpc-test` | `org.springframework.boot.grpc.test.autoconfigure.AutoConfigureTestGrpcTransportTests` |
| `module/spring-boot-micrometer-tracing-test` | `org.springframework.boot.micrometer.tracing.test.autoconfigure.AutoConfigureTracingMissingIntegrationTests` |
| `module/spring-boot-micrometer-tracing-test` | `org.springframework.boot.micrometer.tracing.test.autoconfigure.AutoConfigureTracingPresentIntegrationTests` |
| `module/spring-boot-webmvc-test` | `org.springframework.boot.webmvc.test.autoconfigure.mockmvc.MockMvcSpringBootTestIntegrationTests` (1/6 tests, added bin13) |
| `module/spring-boot-webmvc-test` | `org.springframework.boot.webmvc.test.autoconfigure.mockmvc.MockMvcTesterSpringBootTestIntegrationTests` (1/6 tests, added bin13) |
| `module/spring-boot-micrometer-metrics-test` | `org.springframework.boot.micrometer.metrics.test.autoconfigure.AutoConfigureMetricsMissingIntegrationTests` (added bin7) |
| `module/spring-boot-micrometer-metrics-test` | `org.springframework.boot.micrometer.metrics.test.autoconfigure.AutoConfigureMetricsPresentIntegrationTests` (added bin7) |
