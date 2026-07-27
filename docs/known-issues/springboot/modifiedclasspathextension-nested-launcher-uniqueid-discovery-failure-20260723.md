# `ModifiedClassPathExtension`'s nested `Launcher.discover()` fails to resolve its own `UniqueIdSelector` — `DiscoveryIssueException` on every `@ClassPathExclusions`/`@ClassPathOverrides` test

**Status: OPEN — found 2026-07-23 (craton-rerun-20260723)**

## Symptom

Every test method (or, for class-level annotations, every test method in the
class) annotated `@ClassPathExclusions`/`@ClassPathOverrides` fails —
regardless of module — with the *same* shape: JUnit Platform's own discovery
machinery, not the test body, raises the error. `.err.log` shows nothing
beyond the standard 5-6 line VM-boot "Post-clinit fixup" banner (plus an
occasional unrelated Mockito self-attach warning); `.out.log` shows:

```
ERROR [org.junit.platform.launcher.core.DiscoveryIssueNotifier] TestEngine with ID 'junit-jupiter' encountered a critical issue during test discovery:

(1) [ERROR] UniqueIdSelector [uniqueId = [engine:junit-jupiter]/[class:org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests]/[method:testSecurityFilterDoesNotCauseEarlyInitialization(org.springframework.boot.test.system.CapturedOutput)]] could not be resolved

...
Failures (1):
  JUnit Jupiter:SecurityFilterAutoConfigurationEarlyInitializationTests:testSecurityFilterDoesNotCauseEarlyInitialization(CapturedOutput)
    => org.junit.platform.launcher.core.DiscoveryIssueException: TestEngine with ID 'junit-jupiter' encountered a critical issue during test discovery: ...
```

Notably this happens for both a method that declares a parameter
(`CapturedOutput`) **and** methods that declare none (e.g.
`PathRequestTests.toH2ConsoleWhenNoWebServerContextClassPresent()`), so the
failure is not specific to parameter-type resolution — the class/method
lookup itself fails.

| Module | Class | Failing method(s) | Log |
|---|---|---|---|
| `module/spring-boot-security` | `web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests` | `testSecurityFilterDoesNotCauseEarlyInitialization(CapturedOutput)` (1/1) | `shard1/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.web.servlet.Securi-b33e88968fc2.out.log` |
| `module/spring-boot-security` | `web.servlet.PathRequestTests` | `toH2ConsoleWhenNoWebServerContextClassPresent()` (1/4) | `shard2/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.web.servlet.PathRequestTests.out.log` |
| `module/spring-boot-security` | `actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests` | `securesEverythingElseWhenHealthIsAbsent()` (1/10) | `shard7/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.web.servle-f4cd03f47d82.out.log` |
| `module/spring-boot-security` | `actuate.web.reactive.ReactiveManagementWebSecurityAutoConfigurationTests` | `securesEverythingElseWhenHealthIsAbsent()` (1/9) | `shard5/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.web.reacti-442ba65694db.out.log` |
| `module/spring-boot-micrometer-metrics` | `logging.logback.LogbackMetricsAutoConfigurationWithLog4j2AndLogbackTests` | `doesNotConfigureLogbackMetrics()` (1/1, class-level annotation) | `shard4/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-c49196fdcfa9.out.log` |
| `module/spring-boot-micrometer-metrics` | `logging.log4j2.Log4J2MetricsWithLog4jLoggerContextAutoConfigurationTests` | both methods (2/2, class-level annotation) | `shard5/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-12f21366ced9.out.log` |
| `module/spring-boot-cache` | `EhCache3CacheAutoConfigurationTests` | both methods (2/2, class-level annotation) | `shard7/logs/module_spring-boot-cache.org.springframework.boot.cache.autoconfigure.EhCache3CacheAutoConfigurationTests.out.log` |
| `core/spring-boot-test` | `json.DuplicateJsonObjectContextCustomizerFactoryTests` | `warningForMultipleVersions()` (1/1, class-level annotation) | `shard3/logs/core_spring-boot-test.org.springframework.boot.test.json.DuplicateJsonObjectContextCustomizerFactoryTests.out.log` |

All 8 source files confirmed (via `grep`/`Read` in this worktree,
`apps/spring-boot/.../src/test/java/...`) to carry
`@ClassPathExclusions`/`@ClassPathOverrides` — either class-level (all
methods fail) or on exactly the one failing method (method-level, other
methods in the class pass normally).

## Root cause hypothesis (source-grounded, not confirmed by a debugger attach)

`ModifiedClassPathExtension.interceptMethod()`
(`apps/spring-boot/test-support/spring-boot-test-support/src/main/java/org/springframework/boot/testsupport/classpath/ModifiedClassPathExtension.java:85-108`)
is the mechanism behind every one of these annotations. For an
annotated method it does:

```java
URLClassLoader modifiedClassLoader = ModifiedClassPathClassLoader.get(testClass, testMethod, invocationContext.getArguments());
...
invocation.skip();
Thread.currentThread().setContextClassLoader(modifiedClassLoader);
try {
    runTest(extensionContext.getUniqueId());   // <-- the ORIGINAL method's unique ID
}
...
```

and `runTest` (lines 110-123):

```java
private void runTest(String testId) throws Throwable {
    LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
        .selectors(DiscoverySelectors.selectUniqueId(testId))
        .build();
    Launcher launcher = LauncherFactory.create();
    TestPlan testPlan = launcher.discover(request);
    ...
}
```

i.e. it builds a **brand-new** `Launcher`, sets the current thread's context
classloader to the freshly-built isolated `ModifiedClassPathClassLoader`,
and asks that new `Launcher` to *re-discover* the exact same test method by
its original `UniqueId` string (which embeds the class's binary name and the
method's name/parameter-type descriptor as parsed at the *outer* discovery
pass, i.e. against the class as loaded by the **application** loader).
JUnit Platform's Jupiter engine resolves a `UniqueIdSelector` by loading the
named class via `ReflectionSupport`/`ReflectionUtils` (which consults
`Thread.currentThread().getContextClassLoader()` first) and then matching a
method by name (+ parameter types, when present in the unique ID). "could
not be resolved" is exactly what JUnit Platform emits as a
`DiscoveryIssue`, not a crash, when that lookup fails.

This is the same nested-`Launcher`-via-isolated-loader mechanism at the
center of the now-fixed
[`modifiedclasspath-aether-network-hang-cluster-FIXED.md`](../../internal/fixed-suite-bugs/springboot/modifiedclasspath-aether-network-hang-cluster-FIXED.md)
(originally an infinite-recursion/`StackOverflowError` bug caused by
`isModifiedClassPathClassLoader`'s guard never tripping, fixed 2026-07-19 by
making isolated-loader class resolution throw `NoClassDefFoundError` instead
of silently falling through to CratonVM's flat global class store). **7 of
this doc's 8 classes are explicitly listed in that fix's "Affected classes"
table and were verified passing after the fix** (`SecurityFilterAutoConfigurationEarlyInitializationTests`,
`ManagementWebSecurityAutoConfigurationTests`,
`ReactiveManagementWebSecurityAutoConfigurationTests`, `PathRequestTests`,
`EhCache3CacheAutoConfigurationTests`,
`Log4J2MetricsWithLog4jLoggerContextAutoConfigurationTests`,
`LogbackMetricsAutoConfigurationWithLog4j2AndLogbackTests` —
`DuplicateJsonObjectContextCustomizerFactoryTests` is the one exception, it
was affected by the unrelated, separately-fixed `String.setOption`
wrong-receiver-dispatch bug). **The recursion no longer happens (confirmed —
none of these 8 logs show the old repeating-warning/hang or
`StackOverflowError` shape), but a new failure mode appeared in its place.**

**Working hypothesis for the new failure**, not confirmed by attaching a
debugger: the July 19 fix's isolated-loader hardening
(`resolve_class_loader_aware` in `vm/src/runtime/interpreter.rs`, per that
doc's writeup) makes an isolated loader's failed class resolution throw
immediately instead of silently falling back to the global store. That is
correct for references made *from* a class already defined by the isolated
loader — but the nested `Launcher.discover()` call here resolves the
`UniqueIdSelector`'s class name from **JUnit Platform's own internal code**
(`ReflectionUtils`, defined by the platform/application loader, not by
`modifiedClassLoader`), merely running with the isolated loader set as the
*thread's context classloader*. If CratonVM's isolated-loader gate keys off
something coarser than "is the *referencing class's own defining loader*
isolated" — e.g. also gating on "is the *current thread's context
classloader* isolated" for this reflective `Class.forName`/`loadClass`
entry point — the same hardening that correctly blocks silent cross-loader
fallback for bytecode-level references could now also reject this
legitimate, JUnit-Platform-internal, TCCL-mediated lookup, throwing
`ClassNotFoundException`/`NoClassDefFoundError` where it should succeed
through `modifiedClassLoader`'s own (correctly delegating) `loadClass()` —
which JUnit Platform then reports as an unresolvable `UniqueIdSelector`
rather than propagating as a hard crash.

This would explain every observed detail:
- Method-level-annotated classes fail only their one annotated method
  (only that one method's re-discovery goes through the isolated-loader
  nested `Launcher` path).
- Class-level-annotated classes fail every method (the very first
  `@BeforeAll`/method interception already routes through the isolated
  loader for the whole class).
- No-parameter methods fail exactly like parameterized ones (the break is
  at class *or* method name resolution, not parameter-type matching).

**Not confirmed**: which specific native call/gate is responsible (candidate
starting points: `resolve_class_loader_aware`,
`is_isolated_url_loader_definition`, and whatever native backs
`ClassLoader.loadClass`/`Class.forName` when invoked from JUnit Platform's
reflection utilities with an isolated TCCL — all in
`vm/src/runtime/interpreter.rs` / `native-builtins/src/classloader.rs` /
`native-builtins/src/classloader_real.rs`, per the July 19 fix's own file
list). No debugger attach or temporary trace was added this session to
pinpoint the exact call site — this is a source-grounded hypothesis from
reading `ModifiedClassPathExtension.java` and the prior fix's description,
not a directly observed root cause.

**Not new**: `Log4J2MetricsWithLog4jLoggerContextAutoConfigurationTests` and
`LogbackMetricsAutoConfigurationWithLog4j2AndLogbackTests`'s
`DiscoveryIssueException` was already noted in passing (but not
root-caused or given its own doc) on 2026-07-18 in
[`mockito-inline-nested-selfcall-stub-bypass-FIXED.md`](../../internal/fixed-suite-bugs/springboot/mockito-inline-nested-selfcall-stub-bypass-FIXED.md)'s
regression sweep, flagged there only as "pre-existing, unconnected to
[that doc's] method dispatch fix." This doc is the first dedicated
root-cause investigation.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.web.servlet.PathRequestTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.ReactiveManagementWebSecurityAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.logging.logback.LogbackMetricsAutoConfigurationWithLog4j2AndLogbackTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.logging.log4j2.Log4J2MetricsWithLog4jLoggerContextAutoConfigurationTests` |
| `module/spring-boot-cache` | `org.springframework.boot.cache.autoconfigure.EhCache3CacheAutoConfigurationTests` |
| `core/spring-boot-test` | `org.springframework.boot.test.json.DuplicateJsonObjectContextCustomizerFactoryTests` |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.DataSourceBuilderNoHikariTests` (added 2026-07-23, jdbc-batch triage, 2/2 methods) |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.DataSourceUnwrapperNoSpringJdbcTests` (added 2026-07-23, jdbc-batch triage, 2/2 methods) |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.DataSourceBeanCreationFailureAnalyzerTests` (added 2026-07-23, jdbc-batch triage, 2/2 methods) |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.DataSourceAutoConfigurationWithoutSpringJdbcTests` (added 2026-07-23, jdbc-batch triage, 3/3 methods) |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.HikariDataSourceConfigurationTests` (added 2026-07-23, jdbc-batch triage, 4/13 methods: `configureDataSourceClassNameWithNoEmbeddedDatabaseAvailable`, `whenCheckpointRestoreIsAvailableHikariAutoConfigRegistersLifecycleBean`, `whenCheckpointRestoreIsAvailableAndDataSourceIsFromUserConfigurationHikariAutoConfigRegistersLifecycleBean`, `whenCheckpointRestoreIsAvailableAndDataSourceHasBeenWrappedHikariAutoConfigRegistersLifecycleBean`) |

## Update (2026-07-23, second batch) — 5 more classes, same mechanism, confirmed via source

Independently triaged a `spring-boot-jdbc`/`core-autoconfigure`/`security-oauth2-authorization-server`/`ldap`/`data-elasticsearch` batch
(`apps/spring-boot-suite-runner/.suite/fail-groups/group1.tsv`) before
finding this doc; all 5 added `spring-boot-jdbc` classes above were
source-confirmed (`@ClassPathExclusions`/`@ClassPathOverrides`, grepped
directly in `apps/spring-boot/module/spring-boot-jdbc/src/test/java/...`)
before being added, same method as this doc's original 8. The identical
`DiscoveryIssueException`/`UniqueIdSelector ... could not be resolved`
shape is reproduced exactly, including the class-level-vs-method-level
split (`HikariDataSourceConfigurationTests`'s 4 failing methods are exactly
its 4 `@ClassPathOverrides`/`@ClassPathExclusions`-annotated ones; its other
9 methods pass normally) — strong additional corroboration for this doc's
"nested `Launcher` + isolated TCCL" hypothesis, no new mechanism evidence.
Log paths:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard2/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.DataSourceBuilderNoHikariTests.out.log`,
`.../shard4/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.DataSourceUnwrapperNoSpringJdbcTests.out.log`,
`.../shard5/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.DataSourceBeanCreationFail-31d4ec47a56f.out.log`,
`.../shard5/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.DataSourceAutoConfiguratio-9a0d5d108699.out.log`,
`.../shard3/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.HikariDataSourceConfigurationTests.out.log`
(all under `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/`).
