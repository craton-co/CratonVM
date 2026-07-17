# HttpClient autoconfigure "unavailable dependency" tests pick the wrong builder — classpath-exclusion presence check not honored

**Status: OPEN — found 2026-07-17 (hypothesis, not confirmed against CratonVM source)**

## Symptom

`ImperativeHttpClientAutoConfigurationTests` and
`ReactiveHttpClientAutoConfigurationTests` use
`ModifiedClassPathClassLoader`/`@ClassPathExclusions` to simulate a
dependency (Apache HttpComponents, Jetty, Reactor Netty) being absent from
the classpath, then assert Spring Boot's autoconfiguration falls back to the
next-preferred client builder. In every failing case, CratonVM still picks
the (supposedly excluded) HttpComponents/Reactor builder instead of falling
back — i.e. the classpath-presence check that should report the excluded
class as absent is returning "present" under CratonVM.

| Class | Method | Expected | Actual |
|---|---|---|---|
| `ImperativeHttpClientAutoConfigurationTests` | `whenHttpComponentsAndJettyAndReactorAreUnavailableThenJdkClientBeansAreDefined` | `JdkClientHttpRequestFactoryBuilder` | `HttpComponentsClientHttpRequestFactoryBuilder` |
| same | `whenHttpComponentsAndJettyAreUnavailableThenReactorClientBeansAreDefined` | `ReactorClientHttpRequestFactoryBuilder` | `HttpComponentsClientHttpRequestFactoryBuilder` |
| same | `whenHttpComponentsIsUnavailableThenJettyClientBeansAreDefined` | (not captured in this pass — same shape) | — |
| `ReactiveHttpClientAutoConfigurationTests` | `whenReactorIsUnavailableThenJettyClientBeansAreDefined` | `JettyClientHttpConnectorBuilder` | `ReactorClientHttpConnectorBuilder` |
| same | `whenReactorAndHttpClientAreUnavailableThenJettyClientBeansAreDefined` | `JettyClientHttpConnectorBuilder` | `ReactorClientHttpConnectorBuilder` |
| same | `whenReactorAndHttpClientAndJettyAreUnavailableThenJdkClientBeansAreDefined` | `JdkClientHttpConnectorBuilder` | `ReactorClientHttpConnectorBuilder` |
| same | `shouldBeConditionalOnAtLeastOneHttpConnectorClass` | expects a throwable when *no* connector class is available | none thrown |

Representative trace:

```
JUnit Jupiter:ImperativeHttpClientAutoConfigurationTests:whenHttpComponentsAndJettyAreUnavailableThenReactorClientBeansAreDefined()
    => java.lang.AssertionError:
Expecting actual:
  org.springframework.boot.http.client.HttpComponentsClientHttpRequestFactoryBuilder@240c3
to be exactly an instance of:
  org.springframework.boot.http.client.ReactorClientHttpRequestFactoryBuilder
but was an instance of:
  org.springframework.boot.http.client.HttpComponentsClientHttpRequestFactoryBuilder
       org.springframework.boot.http.client.autoconfigure.imperative.ImperativeHttpClientAutoConfigurationTests.whenHttpComponentsAndJettyAreUnavailableThenReactorClientBeansAreDefined(ImperativeHttpClientAutoConfigurationTests.java:116)
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-http-client.org.springframework.boot.http.client.autoconfigure.imperative.I-58f1291c123e.out.log` (3 of 4 failures — the 4th, `whenVirtualThreadsEnabled...`, is a different bug, see `jdk-httpclient-builder-config-loss-cluster.md`)
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-http-client.org.springframework.boot.http.client.autoconfigure.reactive.Rea-4a8014b6cdc0.out.log` (4 of 5 failures — same carve-out)

## Root cause (hypothesis — not confirmed against CratonVM source this session)

Spring's autoconfiguration for these builders (`ClientHttpRequestFactoryBuilder.detect()`
and its reactive equivalent) picks the first available builder from a
priority-ordered list, using `ClassUtils.isPresent(className, classLoader)`
against the test's classloader — which `@ClassPathExclusions` rigs (via
`ModifiedClassPathClassLoader`) to `ClassNotFoundException` for the excluded
library's marker class. If CratonVM's classloader-isolation/resource-visibility
handling for `ModifiedClassPathClassLoader` does not correctly hide the
excluded classes from a subsequent `Class.forName`/`ClassUtils.isPresent`
check (e.g. resolves them via a parent/system loader instead, or caches
class-presence per class name globally rather than per loader), the
detection logic would see HttpComponents (or Reactor) as available even when
the test intends it to be excluded — explaining every failure in this
cluster uniformly, including `shouldBeConditionalOnAtLeastOneHttpConnectorClass`
(no builder ever becomes fully unavailable, so no `IllegalStateException` is
raised).

This was **not verified against source** this session — no
`ModifiedClassPathClassLoader`/`ClassUtils.isPresent` implementation was
located and inspected. It is a plausible, testable hypothesis based on the
uniform "always picks the earliest-priority builder regardless of which one
should be excluded" shape of every failure, consistent with a
classpath-visibility gap rather than 6 independent autoconfiguration bugs.
Two related-but-not-confirmed-identical docs exist for other
`ModifiedClassPathClassLoader`-adjacent issues found the same day —
`docs/known-issues/springboot/modifiedclasspath-aether-network-hang-cluster.md`
(a HANG, not a FAIL, via real network Aether resolution — different
mechanism) and `docs/known-issues/springboot/core-spring-boot-test-config-data-and-classpath-scan-cluster.md`
Cluster C (a classpath **package-scan** over-enumeration, not a single-class
presence check — also a different mechanism). Neither directly covers
`ClassUtils.isPresent()` returning a false positive for an excluded class;
this is filed as a distinct, new cluster.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.autoconfigure.imperative.ImperativeHttpClientAutoConfigurationTests` (3 of 4 failures) |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.autoconfigure.reactive.ReactiveHttpClientAutoConfigurationTests` (4 of 5 failures) |
