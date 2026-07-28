# `ModifiedClassPathExtension`'s nested UniqueId discovery fails for every test method

**Status: OPEN — found 2026-07-23**

## Symptom

15 of the 34 `core/spring-boot` residuals from the 2026-07-23 rerun (`RunName=craton-rerun-20260723`)
fail with **100% of their test methods** turning into a JUnit-Platform
`DiscoveryIssueException`, e.g.:

```
ERROR [org.junit.platform.launcher.core.DiscoveryIssueNotifier] TestEngine with ID 'junit-jupiter' encountered a critical issue during test discovery:

(1) [ERROR] UniqueIdSelector [uniqueId = [engine:junit-jupiter]/[class:org.springframework.boot.env.NoSnakeYamlPropertySourceLoaderTests]/[method:load()]] could not be resolved

...
Failures (1):
  JUnit Jupiter:NoSnakeYamlPropertySourceLoaderTests:load()
    MethodSource [className = 'org.springframework.boot.env.NoSnakeYamlPropertySourceLoaderTests', methodName = 'load', methodParameterTypes = '']
    => org.junit.platform.launcher.core.DiscoveryIssueException: TestEngine with ID 'junit-jupiter' encountered a critical issue during test discovery:

(1) [ERROR] UniqueIdSelector [uniqueId = [engine:junit-jupiter]/[class:org.springframework.boot.env.NoSnakeYamlPropertySourceLoaderTests]/[method:load()]] could not be resolved
SBRUNNER_RESULT tests=1 failed=1 aborted=0 skipped=0 containersFailed=0
```

Note the outer container discovery ("3 containers found / 3 containers
successful / 0 containers failed") succeeds — only the *method-level*
resolution inside a nested discovery pass fails, for every single test
method in the class, with no exceptions.

Full logs, e.g.:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard4/logs/core_spring-boot.org.springframework.boot.web.servlet.NoSpringWebFilterRegistrationBeanTests.out.log` (19/19 methods fail this way)
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard4/logs/core_spring-boot.org.springframework.boot.context.logging.LoggingApplicationListenerTests.out.log` (41/41 methods fail this way)

## Root cause

Every affected class is annotated with Spring Boot's
`@ClassPathExclusions`, `@ClassPathOverrides`, or `@ForkedClassPath`
(`apps/spring-boot/test-support/spring-boot-test-support/.../testsupport/classpath/`).
These are handled by `ModifiedClassPathExtension`
(`ModifiedClassPathExtension.java:85-123`), which — for every
`@BeforeAll`/`@BeforeEach`/`@Test`/`@AfterEach`/`@AfterAll` invocation —
builds a fresh `ModifiedClassPathClassLoader` (a `URLClassLoader` with the
excluded/overridden jars filtered from the URL list), sets it as the
thread's context classloader, and then re-runs test discovery **from
scratch, in-process**, via:

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

`testId` is the *string* form of the UniqueId built by the **outer**
(first, real-classloader) discovery pass — e.g.
`[engine:junit-jupiter]/[class:...NoSnakeYamlPropertySourceLoaderTests]/[method:load()]`.
This nested `Launcher.discover()` call runs with the thread context
classloader switched to the new `ModifiedClassPathClassLoader`, so
resolving the `[class:...]` segment loads a **second copy** of the test
class (JUnit/Hamcrest/Netty-tcnative classes are explicitly redirected
back to the original loader by `ModifiedClassPathClassLoader.loadClass`,
lines 90-100, but the test class itself is not — it is freshly defined
under the new loader). Resolving the `[method:...]` segment against that
freshly-loaded second class copy then fails, for every method, on every
one of the 15 affected classes, 100% of the time.

This is the same JUnit-Platform-discovery-error shape (`DiscoveryIssueException:
... could not be resolved`) documented once before, in
`../../internal/fixed-suite-bugs/springboot/core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md`
(2026-07-20 update), where it was attributed to **flaky Aether/`.m2`
network contention** under parallel-shard load for a `@ClassPathOverrides`
class that resolves a Maven artifact. That attribution does not fit this
occurrence: none of the 15 classes here need Aether at all (they all use
plain `@ClassPathExclusions`, e.g. `@ClassPathExclusions("snakeyaml-*.jar")`,
`@ClassPathExclusions("log4j*.jar")` — pure local-jar filtering, no
network I/O), and the failure rate is 100% of methods across 100% of these
15 classes rather than one flaky class under heavy parallel load. **The
2026-07-20 doc's stated root cause does not explain this occurrence** — this
looks like a distinct, deterministic bug in how CratonVM resolves a
`[method:...]` UniqueId segment against a class that was defined a
*second time*, under a *second* `URLClassLoader`, from within the same
running process.

Not pinned to an exact file:line in CratonVM's reflection/class-registration
code — the most likely mechanism, based on the project's known "dual class
copy" and class-registration-bookkeeping gaps (see
`reference_dual_registration_classloader_vs_classloader_real.md` and
`reference_multiple_class_copies_are_normal_under_isolating_loaders.md` in
memory), is that CratonVM's declared-method reflection lookup
(`getDeclaredMethod`/`getDeclaredMethods`, `native-builtins/src/lang_class.rs`
/ `lang_reflect.rs`) or its class-registration bookkeeping
(`classloader.rs` vs `classloader_real.rs`) does not correctly index
methods for a class defined a second time via a plain `URLClassLoader`
once a class of the same fully-qualified name already exists under a
different loader in the process.

**What would confirm/refute:** a minimal 2-classloader repro — define the
same class under two sibling `URLClassLoader`s in one process and call
`Class.getDeclaredMethod("foo")` on the second copy. If that also fails,
the bug is in reflection/class-registration proper. If it succeeds, the
gap is instead somewhere specific to how JUnit Jupiter's internal
`MethodSelectorResolver` interacts with a class reached via
`DiscoverySelectors.selectUniqueId` + a swapped context classloader.

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.logging.log4j2.SpringBootPropertySourceTests |
| core/spring-boot | org.springframework.boot.diagnostics.analyzer.JakartaApiValidationExceptionFailureAnalyzerTests |
| core/spring-boot | org.springframework.boot.SpringApplicationNoWebTests |
| core/spring-boot | org.springframework.boot.context.logging.LoggingApplicationListenerTests |
| core/spring-boot | org.springframework.boot.web.servlet.NoSpringWebFilterRegistrationBeanTests |
| core/spring-boot | org.springframework.boot.env.NoSnakeYamlPropertySourceLoaderTests |
| core/spring-boot | org.springframework.boot.logging.log4j2.SpringProfileArbiterTests |
| core/spring-boot | org.springframework.boot.logging.LogbackAndLog4J2ExcludedLoggingSystemTests |
| core/spring-boot | org.springframework.boot.logging.logback.LogbackLoggingSystemParallelInitializationTests |
| core/spring-boot | org.springframework.boot.diagnostics.analyzer.NoSuchMethodFailureAnalyzerTests |
| core/spring-boot | org.springframework.boot.validation.MessageInterpolatorFactoryWithoutElIntegrationTests |
| core/spring-boot | org.springframework.boot.logging.logback.SpringBootJoranConfiguratorTests |
| core/spring-boot | org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests |
| core/spring-boot | org.springframework.boot.logging.logback.LogbackLoggingSystemTests |
| core/spring-boot | org.springframework.boot.logging.LoggingSystemTests |
