# `MicrometerTracingAutoConfigurationTests` — classpath condition wrongly matches through a `FilteredClassLoader`

**Status: OPEN — found 2026-07-17, not root-caused**

## Symptom

Module `module/spring-boot-micrometer-tracing`, class
`MicrometerTracingAutoConfigurationTests`, test
`shouldCreateTracingObservationHandlerGroupWhenMetricsIsNotOnClassPath()`
(1 of 11 failures):

```
=> java.lang.AssertionError:
Expecting actual:
  org.springframework.boot.micrometer.tracing.autoconfigure.TracingAndMeterObservationHandlerGroup@23d72
not to be an instance of: org.springframework.boot.micrometer.tracing.autoconfigure.TracingAndMeterObservationHandlerGroup
       org.springframework.boot.micrometer.tracing.autoconfigure.MicrometerTracingAutoConfigurationTests.lambda$shouldCreateTracingObservationHandlerGroupWhenMetricsIsNotOnClassPath$0(MicrometerTracingAutoConfigurationTests.java:205)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-tracing.org.springframework.boot.micrometer.tracing.autoconfigur-8265a9df7595.out.log`

## What the test actually does (confirmed from source)

`apps/spring-boot/module/spring-boot-micrometer-tracing/src/test/java/.../MicrometerTracingAutoConfigurationTests.java:199-209`:

```java
void shouldCreateTracingObservationHandlerGroupWhenMetricsIsNotOnClassPath() {
    this.contextRunner.withUserConfiguration(TracerConfiguration.class)
        .withClassLoader(new FilteredClassLoader("io.micrometer.core"))
        .run((context) -> {
            assertThat(context).hasSingleBean(ObservationHandlerGroup.class);
            ObservationHandlerGroup group = context.getBean(ObservationHandlerGroup.class);
            assertThat(group).isNotInstanceOf(TracingAndMeterObservationHandlerGroup.class);
            ...
        });
}
```

`FilteredClassLoader("io.micrometer.core")` is Spring Boot's test-support
classloader that makes any class under the `io.micrometer.core` package
appear absent (`ClassNotFoundException`) to whatever code queries it. The
test's premise is: with `io.micrometer.core`'s `Meter`/metrics classes
hidden, the autoconfiguration's `@ConditionalOnClass` guard for the
metrics-aware `TracingAndMeterObservationHandlerGroup` variant should NOT
match, so a plain `ObservationHandlerGroup` (tracing-only) should be created
instead. CratonVM instead creates the metrics-aware
`TracingAndMeterObservationHandlerGroup` — i.e. **the classpath condition
matched as if `io.micrometer.core` classes were still visible**, despite the
test explicitly hiding them via a custom `ClassLoader` passed to the context
runner.

## Root cause

**Not confirmed.** `FilteredClassLoader` itself is ordinary Spring
test-support bytecode (a real `ClassLoader` subclass overriding
`loadClass`), so this should "just work" through normal classloader
delegation once the `ApplicationContextRunner` installs it as the context's
class loader. Grepped `native-builtins/src/*.rs`,
`native-collections/src/lib.rs`, and `vm/src/**/*.rs` for
`FilteredClassLoader`: **zero hits** anywhere in CratonVM's own source —
meaning there's no CratonVM-side special-casing of this class, which itself
doesn't rule out a bug (a general classpath-condition implementation issue
could still be at fault without ever naming `FilteredClassLoader`
specifically).

Strongest hypothesis (unconfirmed, no source-level tracing done this
session): the relevant `@ConditionalOnClass`/`OnClassCondition` evaluation
path resolves class presence via a mechanism that doesn't actually delegate
through the *context-specific* classloader passed into
`ApplicationContextRunner.withClassLoader(...)` — e.g. consulting a
VM-global "is this class known/loadable" registry or the boot/system
classloader instead of the merged bean-definition's resolved classloader.
This would be in the same general family as this suite's already-`FIXED`
`OnClassCondition.addAll` NPE-cast-`String[]` cluster (see
`docs/known-issues/springboot/README.md`'s first table row) and the still-`OPEN`
`comparable-classcast-lambda-proxy-unknown-class.md`
(`class_manager`-only lookups missing dynamically-scoped class visibility) —
both are, at a high level, "classpath/class-visibility resolution doesn't
fully respect the caller's actual classloader/registration scope." (The
latter is now `RESOLVED` — see
`docs/internal/comparable-classcast-lambda-proxy-unknown-class-RESOLVED.md`
— but its fix was specific to lambda-proxy `Comparable` checks in
`native-collections`, a different call site than whatever backs
`@ConditionalOnClass` here.) Not confirmed to be the same underlying code
path as either.

**Next step for whoever picks this up:** a standalone repro constructing a
`FilteredClassLoader` and calling `ClassUtils.isPresent("io.micrometer.core.instrument.observation.MeterObservationHandler",
filteredLoader)` (or whatever the actual guarding condition class is)
directly, without a full Spring context, would confirm whether the gap is
in Spring's own condition-evaluation bytecode misbehaving under CratonVM,
or in a CratonVM-native shortcut for class-presence checks that bypasses
the passed-in loader.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-micrometer-tracing` | `org.springframework.boot.micrometer.tracing.autoconfigure.MicrometerTracingAutoConfigurationTests` (1 of 11 failing test methods: `shouldCreateTracingObservationHandlerGroupWhenMetricsIsNotOnClassPath`) |
