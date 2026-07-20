# `@ConditionalOnMissingBean` misses a user bean under `@ClassPathExclusions`'s forked classloader

**Status: OPEN — found 2026-07-20**

## Symptom

`RestClientObservationAutoConfigurationWithoutMetricsTests` and
`RestTemplateObservationAutoConfigurationWithoutMetricsTests`
(`module/spring-boot-restclient`) both fail their single test with:

```
JUnit Jupiter:RestClientObservationAutoConfigurationWithoutMetricsTests:restClientCreatedWithBuilderIsInstrumented()
    => java.lang.IllegalStateException: Unstarted application context ...[startupFailure=org.springframework.beans.factory.support.BeanDefinitionOverrideException] failed to start
     Caused by: org.springframework.beans.factory.support.BeanDefinitionOverrideException: Invalid bean definition with name 'observationRegistry' defined in org.springframework.boot.micrometer.observation.autoconfigure.ObservationAutoConfiguration: @Bean definition illegally overridden by existing bean definition: Generic bean: class=io.micrometer.observation.ObservationRegistry; scope=singleton; ...
```

This is a **new** residual, discovered while verifying that the
`InterceptingExecutableInvoker` livelock fix
([`spring-boot-restclient-residuals-FIXED.md`](../../internal/springboot/spring-boot-restclient-residuals-FIXED.md))
holds — both classes previously HANG-timed-out before ever reaching this
code path, so this bug was invisible until the hang was fixed 2026-07-18.
Reproduces identically with `-Jit off` (`--nojit`), ruling out a JIT
speculation issue.

## Root cause (not yet found — isolated but not fixed)

The test context is:

```java
private final ApplicationContextRunner contextRunner = new ApplicationContextRunner()
    .withBean(ObservationRegistry.class, TestObservationRegistry::create)
    .withConfiguration(AutoConfigurations.of(ObservationAutoConfiguration.class, RestClientAutoConfiguration.class,
            RestClientObservationAutoConfiguration.class));
```

`ObservationAutoConfiguration.observationRegistry()` is
`@ConditionalOnMissingBean` (bean type inferred from its `ObservationRegistry`
return type — `apps/spring-boot/module/spring-boot-micrometer-observation/src/main/java/.../ObservationAutoConfiguration.java:68-71`).
With a bean of that exact type already registered via `.withBean(...)`, the
condition should skip registering the auto-configured
`observationRegistry()` bean entirely — and on real HotSpot it does (no
`BeanDefinitionOverrideException` is possible unless the condition
evaluates "missing" incorrectly, since `ConfigurationClassBeanDefinitionReader
.isOverriddenByExistingDefinition`, where this exception originates, only
runs for a bean method that condition evaluation already decided to keep).

**Isolated the trigger to `@ClassPathExclusions`, not the bean pattern
itself:** the sibling test `RestClientObservationAutoConfigurationTests`
(same module, same `.withBean(ObservationRegistry.class,
TestObservationRegistry::create)` call, same `AutoConfigurations.of(...)`
set, **no** `@ClassPathExclusions`) passes cleanly — 5/5 PASS, verified on
the same build. The only difference between the two test classes is
`@ClassPathExclusions("micrometer-core-*.jar")` on the failing ones, which
routes test execution through
`org.springframework.boot.testsupport.classpath.ModifiedClassPathExtension`
— a JUnit5 extension that builds a `ModifiedClassPathClassLoader` (a
`URLClassLoader` subclass excluding the named jar's packages), swaps the
thread's context classloader to it, and re-runs the entire test
method through a **new, nested** `Launcher.discover()`+`execute()` call
(`ModifiedClassPathExtension.runTest`) — the same mechanism responsible for
Issue A's now-fixed livelock in the sibling doc above, and for the
already-known nested-Launcher-execution complexity documented there.

Working hypothesis (not confirmed): `@ConditionalOnMissingBean`'s type match
for `ObservationRegistry` fails to recognize the `.withBean(...)`-registered
instance as satisfying the condition when both are evaluated under/through
`ModifiedClassPathClassLoader` — plausibly a classloader-identity mismatch
(two different `Class` objects for the same binary name, one loaded via the
forked child loader and one via its parent), though a manual read of
`native-builtins/src/classloader.rs`'s `cl_load_class_base_delegation` /
`find_loaded_class_for_loader` parent-delegation logic did not turn up an
obvious bug for this specific loader shape (non-null parent = the real
application loader, no `findClass` override interacting with the
bootstrap-scoped defer logic). `ModifiedClassPathClassLoader` is
constructed with `super(urls, parent)` where `parent` is the test class's
own loader (not null), so JVMS parent-first delegation should apply
normally and `io.micrometer.observation.ObservationRegistry` (not itself
excluded — only `micrometer-core-*.jar` is) should resolve to the parent's
already-loaded copy either way.

An equally plausible alternative not yet ruled out: `@ConditionalOnMissingBean`
with no explicit type reads the `@Bean` method's return type via **ASM**
bytecode parsing (`MethodMetadata`), a completely different code path from
`ClassLoader.loadClass` — if CratonVM's condition-evaluation / bean-type
registry does its existing-bean type comparison by name-string rather than
by resolved `Class` identity in some sub-path, a forked-classloader-specific
bug there wouldn't show up in `classloader.rs` at all.

**Not investigated:** `OnBeanCondition`/`ConditionEvaluationReport`/
`BeanTypeRegistry` (Spring Framework 7 unmodified code, but CratonVM's
handling of the beans/conditions it inspects could still be the fault),
and whether other `@ClassPathExclusions` classes with a similarly-typed
user-supplied singleton bean show the same failure (only these two were
checked this session — most of the ~25-class
`ModifiedClassPathExtension` cluster from the livelock doc are
DataSource/JSON-provider-style autoconfiguration tests without an
equivalent `.withBean(SameTypeAsAutoConfiguredBean, ...)` pattern, so this
may be narrower than "every `@ClassPathExclusions` test").

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Jit on -TimeoutSec 120 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <a TSV with header "module`tclass" and the 2 rows below>
```
```
module	class
module/spring-boot-restclient	org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests
module/spring-boot-restclient	org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests
```

Control (should PASS, to confirm the `@ClassPathExclusions` isolation still
holds before further investigation):

```
module	class
module/spring-boot-restclient	org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationTests
```

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests` |
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests` |
