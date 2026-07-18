# JUnit5 `InterceptingExecutableInvoker` speculative-layout-probe livelock (HANG cluster)

**Status: FIXED 2026-07-18**

## Resolution

The apparent collection-layout probe was actually the foreign-function downcall adapter fast path in `try_stackless_invoke`. It was selected solely by the method name `invoke`/`invokeExact`/`invokeBasic` and read receiver field zero before proving that the receiver was a `MethodHandle`. JUnit's zero-field `InterceptingExecutableInvoker.invoke` therefore entered that unrelated path, repeatedly generated a guarded out-of-bounds field read, and could make no forward progress.

The adapter fast path now first establishes, from the runtime receiver class hierarchy, that its receiver is `java/lang/invoke/MethodHandle` or a subclass. All other same-named methods return to ordinary invocation without probing a field. This is deliberately a receiver-type check rather than a declaring-method-name check because an inherited MethodHandle method may be resolved on a different owner than the concrete adapter receiver.

The focused 30-class Spring Boot cluster was re-run with the debug guard enabled after the change: there were zero JUnit `InterceptingExecutableInvoker` out-of-bounds probes. The original five representative classes also had zero such probes under `--nojit`. Formerly hung Redis and WebSocket cases now complete with their ordinary, unrelated test outcomes instead of timing out.

`ThreadDumpEndpointTests` exposed a separate liveness residual while making this verification run: real-JDK `Thread.getState()` only reported NEW, RUNNABLE, or TERMINATED, so its setup loop could never observe WAITING or BLOCKED. The VM now publishes WAITING for wait/park/join regions and BLOCKED for contended monitor entry, which makes that test complete in both execution modes. Its remaining JMX lock/monitor-text assertion is a distinct, pre-existing `ThreadInfo` fidelity limitation, recorded in [`../../known-issues/springboot/thread-dump-endpoint-jmx-threadinfo-fidelity.md`](../../known-issues/springboot/thread-dump-endpoint-jmx-threadinfo-fidelity.md).

## Symptom

5 classes across 3 modules HANG (killed by suite timeout, no
`SBRUNNER_RESULT` line, no JUnit output at all beyond the Spring Boot
banner):

| Module | Class | Log |
|---|---|---|
| `module/spring-boot-devtools` | `DevToolsEmbeddedDataSourceAutoConfigurationTests` | `shard3/logs/module_spring-boot-devtools.org.springframework.boot.devtools.autoconfigure.DevToolsEmbeddedDa-251ffbbaec3e.err.log` |
| `module/spring-boot-devtools` | `DevToolsR2dbcAutoConfigurationTests` | `shard3/logs/module_spring-boot-devtools.org.springframework.boot.devtools.autoconfigure.DevToolsR2dbcAutoC-f326922fd6d6.err.log` |
| `module/spring-boot-devtools` | `DevToolPropertiesIntegrationTests` | `shard3/logs/module_spring-boot-devtools.org.springframework.boot.devtools.env.DevToolPropertiesIntegrationTests.err.log` |
| `module/spring-boot-servlet` | `MultipartAutoConfigurationTests` | `shard5/logs/module_spring-boot-servlet.org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests.err.log` |
| `module/spring-boot-jsonb` | `JsonbAutoConfigurationWithNoProviderTests` | `shard4/logs/module_spring-boot-jsonb.org.springframework.boot.jsonb.autoconfigure.JsonbAutoConfigurationWi-fceace83123b.err.log` |

All 5 `.err.log` files are dominated, from very early in the run until the
process is killed, by the same repeating warning (this example from
`DevToolsEmbeddedDataSourceAutoConfigurationTests`, minutes-long tail
identical in shape):

```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matching receiver type) obj=0x16f4d8b8a68 index=0 num_slots=0 class_id=ClassId(735) class_name=org/junit/jupiter/engine/execution/InterceptingExecutableInvoker real_field_count=Some(0)
```

The distinguishing feature versus ordinary occurrences of this warning
(present in 466/510 = 91% of this round's logs overall, including many
`PASS`es — see `crashfail-20260717-crash-cluster.md` in this directory) is
**cadence**: in these 5 logs the *same handful of object addresses*
(`org/junit/jupiter/engine/execution/InterceptingExecutableInvoker` and
`InvocationInterceptorChain`, both real classes with 0 fields) are re-probed
in a steady, unbroken rhythm — pairs of warnings ~150-400ms apart, then a
~1.2-1.5s gap, repeating for the entire lifetime of the process (230-520+
lines per log, spanning the full suite timeout) with **no other log output
at all** — not even the second Spring Boot banner line, no JUnit container
events, no `SBRUNNER_RESULT`. That shape (identical objects, identical
class, fixed periodicity, zero forward progress) is the signature of a
livelock, not the usual one-off/rare occurrence of this guard.

`RabbitAutoConfigurationTests` (`module/spring-boot-amqp`, also a HANG in
this batch) shows the same warning early on but then transitions to
TLS/CGLIB-enhancement log lines and stops there — a different shape,
**not** part of this cluster; documented separately in
`rabbitautoconfigurationtests-broker-connect-hang.md`.

**Cross-reference (2026-07-17, parallel triage, same rerun):** 2 more
classes matching this exact signature — `.err.log` dominated from the very
first log line by the same warning, alternating between exactly 2 fixed
object addresses at a tight ~100-300ms period, zero JUnit output for the
whole run — were found independently in `module/spring-boot-restclient`:
`RestClientObservationAutoConfigurationWithoutMetricsTests` and
`RestTemplateObservationAutoConfigurationWithoutMetricsTests`. See
[`spring-boot-restclient-residuals.md`](spring-boot-restclient-residuals.md)
(Issue A) for their full log excerpts and paths — folding the
cross-reference here rather than duplicating the excerpt. This raises the
cluster to 7 classes across 4 modules, reinforcing that the trigger is tied
to some property of interceptor-chain execution timing rather than the
classes' own test-body content (restclient's `*ObservationAutoConfigurationWithoutMetricsTests`
share no obvious Java-level shape with the devtools/servlet/jsonb classes
above either).

**Update (2026-07-17, later same-day triage pass, bin7): found the missing
common Java-level shape — `ModifiedClassPathExtension`.** 4 more classes
with the byte-identical signature (same repeating `InterceptingExecutableInvoker`/
`InvocationInterceptorChain` `gc::guard` warning, zero JUnit output, killed
at shard timeout, no `SBRUNNER_RESULT`):

| Module | Class | Log |
|---|---|---|
| `module/spring-boot-data-redis` | `DataRedisAutoConfigurationJedisTests` | `shard2/logs/module_spring-boot-data-redis.org.springframework.boot.data.redis.autoconfigure.DataRedisAutoC-7b09ad148f2a.err.log` |
| `module/spring-boot-data-redis` | `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests` | `shard2/logs/module_spring-boot-data-redis.org.springframework.boot.data.redis.autoconfigure.DataRedisAutoC-77c693e1f2c7.err.log` |
| `module/spring-boot-data-redis` | `health.DataRedisHealthContributorAutoConfigurationTests` | `shard2/logs/module_spring-boot-data-redis.org.springframework.boot.data.redis.autoconfigure.health.DataRed-dc6ecb50c06b.err.log` |
| `module/spring-boot-websocket` | `servlet.WebSocketMessagingAutoConfigurationTests` | `shard5/logs/module_spring-boot-websocket.org.springframework.boot.websocket.autoconfigure.servlet.WebSocke-06b735f8a994.err.log` |

Checking source for these 4 plus the original 7 (this doc's devtools/servlet/jsonb
5 + restclient's 2) turns up a strong partial correlation that the earlier
entries missed because nobody had checked source for the devtools/servlet/jsonb
classes yet: **all 11 of these 11 classes import and use
`org.springframework.boot.testsupport.classpath.ClassPathExclusions`,
`ClassPathOverrides`, or `ForkedClassPath`** — the three annotations that
drive JUnit5's `ModifiedClassPathExtension` (`interceptTestMethod`/
`interceptBeforeAllMethod`/etc. in
`apps/spring-boot/test-support/spring-boot-test-support/.../ModifiedClassPathExtension.java`).
**Caveat, reconciling with the entries added below by a concurrent
session:** this correlation does **not** extend to the whole doc —
`TomcatServletWebServerServletContextListenerTests`,
`OpenTelemetryPropertiesTests`, and `DataJpaRepositoriesAutoConfigurationTests`
(added further down, same day) were checked and confirmed to use **none**
of these annotations, yet show the byte-identical hang signature. So the
9-modules/14-classes population in this doc as a whole is **not**
uniformly explained by `ModifiedClassPathExtension` — it looks like either
(a) two distinct bugs that happen to produce an identical guard-warning
signature (a real, confirmed `ModifiedClassPathExtension`
nested-Launcher-execution livelock for the 11 classes below, coincidentally
sharing its log shape with a separate, still-unexplained livelock for
Tomcat/OTel/JPA), or (b) one bug whose trigger condition is broader than
"uses `ModifiedClassPathExtension`" and these 11 classes are simply the
subset that happens to also use it. Not distinguished this session — flagging
both readings rather than picking one:

| Class | Annotation (confirmed via grep of the actual test source) |
|---|---|
| `DevToolsEmbeddedDataSourceAutoConfigurationTests` | `@ClassPathExclusions("HikariCP-*.jar")` |
| `DevToolsR2dbcAutoConfigurationTests` | `@ClassPathExclusions("r2dbc-pool*.jar")` |
| `DevToolPropertiesIntegrationTests` | `@ForkedClassPath` |
| `MultipartAutoConfigurationTests` | `@ForkedClassPath` |
| `JsonbAutoConfigurationWithNoProviderTests` | `@ClassPathExclusions("yasson-*.jar")` |
| `RestClientObservationAutoConfigurationWithoutMetricsTests` | `@ClassPathExclusions("micrometer-core-*.jar")` |
| `RestTemplateObservationAutoConfigurationWithoutMetricsTests` | `@ClassPathExclusions("micrometer-core-*.jar")` |
| `DataRedisAutoConfigurationJedisTests` | `@ClassPathExclusions("lettuce-core-*.jar")` |
| `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests` | `@ClassPathExclusions("commons-pool2-*.jar")` |
| `health.DataRedisHealthContributorAutoConfigurationTests` | `@ClassPathExclusions({"reactor-core*.jar","lettuce-core*.jar"})` |
| `servlet.WebSocketMessagingAutoConfigurationTests` (2 test methods) | `@ClassPathExclusions("jackson-*-3*")` |

This **overturns** this doc's earlier "no obvious common Java-level shape"
conclusion. `ModifiedClassPathExtension.interceptMethod` (for
`@ClassPathExclusions`/`@ClassPathOverrides`, on `@Test`/`@TestTemplate`
methods) or `intercept` (for `@ForkedClassPath`, on lifecycle methods) does:
build/reuse a cached `ModifiedClassPathClassLoader`, swap the thread's
context classloader, then **recursively re-run the entire JUnit Platform
discovery+execution machinery in-process** (`ModifiedClassPathExtension.runTest`):
```java
private void runTest(String testId) throws Throwable {
    LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
        .selectors(DiscoverySelectors.selectUniqueId(testId)).build();
    Launcher launcher = LauncherFactory.create();
    TestPlan testPlan = launcher.discover(request);
    ...
    launcher.execute(testPlan);
    ...
}
```
— a brand-new `Launcher` instance runs `discover()`+`execute()` for the
same test **while the outer Launcher's own `execute()` call is still on the
call stack** (this all happens inside `interceptTestMethod`, itself invoked
by the outer `InterceptingExecutableInvoker`/`InvocationInterceptorChain`).
This nested/reentrant Launcher execution — present for every one of the 11
classes above, absent from ordinary tests — is the strongest common
denominator found so far for this livelock, stronger than the previous
"speculative JIT probe with no Java-level correlation" framing. It doesn't
by itself explain *why* the reentrant call specifically livelocks on
`InterceptingExecutableInvoker`/`InvocationInterceptorChain` object reads
rather than some other reentrant JUnit-internal object — that part of the
mechanism (a reentrant lock, a per-thread cache keyed by class rather than
by classloader, or the same speculative-collection-probe theory now scoped
to "triggered specifically by nested Launcher execution") is still
unconfirmed. **Refutes a competing hypothesis filed the same day**,
[`modifiedclasspath-aether-network-hang-cluster.md`](modifiedclasspath-aether-network-hang-cluster.md)
("the hang is a real outbound network request via Eclipse Aether") — 9 of
these 11 classes use `@ClassPathExclusions`/`@ForkedClassPath` only, never
`@ClassPathOverrides`, and `ModifiedClassPathClassLoader.getAdditionalUrls`
(source-confirmed, `.../ModifiedClassPathClassLoader.java:232-241`) only
calls Aether's `resolveCoordinates` when `@ClassPathOverrides` is present —
so most of these hangs cannot involve any network call at all. See that
doc for the details of the refutation; it has not been deleted/merged here
per this session's instructions (no cross-doc restructuring), but the
nested-Launcher-execution theory above should be treated as superseding its
network hypothesis for the classes they both list.

**Cross-reference (2026-07-17, separate triage batch, same rerun):** one more
class matching this exact signature — `.err.log` dominated from very early on
by the same `InterceptingExecutableInvoker` guard warning (`class_id=734`
this time), zero `INFO`/JUnit output for the entire ~103s captured window,
alternating between exactly 2 fixed object addresses at a tight ~1-2s
period — found in `module/spring-boot-tomcat`:
`org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests`
(`extends AbstractServletWebServerServletContextListenerTests`). Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.servlet.TomcatServletWebServerServle-bd5a7331703d.err.log`.
This raises the cluster to **8 classes across 5 modules**. Notably this class
does NOT use `@ClassPathExclusions`/`@ClassPathOverrides` (ruled out by
reading the test source), so it's independent evidence against the trigger
being specific to `ModifiedClassPathExtension` (contrast with the *separate*,
confirmed-mechanism `ModifiedClassPathExtension` recursive-self-invocation
cluster in `modifiedclasspath-aether-network-hang-cluster.md`, which shares
similar guard-warning noise but a different, source-confirmed root cause —
the two should not be conflated despite the superficially similar log
signature).

**Cross-reference (2026-07-17, bin2 rerun triage):** 2 more classes match
this exact signature (zero JUnit output, `InterceptingExecutableInvoker`/
`InvocationInterceptorChain`, `num_slots=0`, steady sub-2s-interval
repetition for the process's entire lifetime, no `SBRUNNER_RESULT`):

- `module/spring-boot-opentelemetry` `OpenTelemetryPropertiesTests` — `.out.log` is 0 bytes; `.err.log` alternates between 2 fixed object addresses (`ClassId(722)`/`ClassId(723)`) at a ~0.3-2s period for the whole run. Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-opentelemetry.org.springframework.boot.opentelemetry.autoconfigure.OpenTele-5045251e7454.err.log`
- `module/spring-boot-data-jpa` `DataJpaRepositoriesAutoConfigurationTests` — `.out.log` is 0 bytes; `.err.log` alternates between 2 fixed object addresses (`ClassId(748)`) at a similar sub-2s period. Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-jpa.org.springframework.boot.data.jpa.autoconfigure.DataJpaRepositorie-31f7cc414ffe.err.log`

This raises the cluster to 9 classes across 6 modules, further reinforcing
that the trigger has no obvious common Java-level shape across affected
classes (an OTel properties-parsing test and a Spring Data JPA
autoconfiguration test share nothing at the test-body level either).

**Also checked and NOT added** (considered but signature doesn't match):
`module/spring-boot-pulsar`'s `PulsarAutoConfigurationTests` HANGs with the
same "zero JUnit output, steady warning repetition" outer shape, but the
repeating warning is against a *different* receiver class
(`org/springframework/core/$Proxy37`/`$Proxy38`, `num_slots=1`, not
`InterceptingExecutableInvoker`/`num_slots=0`) at a much tighter (sub-second,
many-per-second) cadence — left unclustered pending its own investigation
rather than folded in here on a superficial match. 3 of this same rerun
batch's `module/spring-boot-jetty` HANGs
(`JettyWebServerFactoryCustomizerTests`,
`JettyServletWebServerServletContextListenerTests`,
`JettyServletWebServerFactoryTests`) also have zero JUnit output, but their
`InterceptingExecutableInvoker`/`$Proxy*` warnings are separated by
multi-minute gaps rather than this cluster's steady sub-2s rhythm — a
sparser pattern consistent with slow forward progress rather than a tight
livelock; see `jetty-private-lambda-wrong-receiver-startcontext-recursion-cluster.md`
for that separate hypothesis.

## Root cause

**Not confirmed — strong hypothesis grounded in the guard's own diagnostic
text.**

The warning is emitted by `cratonvm::gc::guard` (`gen_heap::get_field`) when
a caller reads field slot `index` off an object whose real layout has
`num_slots=0` — i.e. some caller is treating a zero-field JUnit5 internal
object (`InterceptingExecutableInvoker` / `InvocationInterceptorChain`,
`ClassId(735)`/`ClassId(736)` in this run) as if it had at least one slot,
consistent with the guard's own description: "typically a speculative
collection-layout probe dispatched on a non-matching receiver type" — i.e.
some hot-path optimization (most likely a JIT-compiled or interpreter
fast-path for a Collection/Map-shaped receiver, per the existing analysis
in `crashfail-20260717-crash-cluster.md`) is being invoked repeatedly with
one of these JUnit5 objects as the receiver.

The guard "drops" the out-of-bounds read (returns a safe default rather
than corrupting memory or crashing), which is why the process doesn't
crash — but in the 5 classes here, whatever loop depends on that probe
succeeding never observes a value that lets it exit, so it retries forever
at a fixed interval instead of ever reaching `InterceptingExecutableInvoker`'s
real invocation logic (`invoke`/`invokeVoid`, which is exactly the method
JUnit5 uses to call `@Test`/`@BeforeEach`/`@AfterEach` methods through the
interceptor chain — see the stack frames in the unrelated
`NoClassDefFoundError: ExceptionUtils` cluster in the same directory, which
also names this class). This would mean: any test whose invocation path
happens to trigger this particular speculative probe on
`InterceptingExecutableInvoker`/`InvocationInterceptorChain` hangs before
its first `@Test` method (or even `@BeforeEach`) ever runs, which matches
all 5 logs having **zero** JUnit output.

Not identified: which specific caller issues the speculative probe (would
require either a live repro with `CRATONVM_DBG_JIT_DISASM` to catch the
compiled code doing the probing, or bisecting with `--nojit` to confirm/
refute JIT involvement — the same next step the sibling
`NoClassDefFoundError: ExceptionUtils` cluster doc recommends and did not
attempt this round). Given 4 of the 5 affected classes are DataSource/R2DBC/
JSON-provider/multipart auto-configuration tests with no obvious common
Java-level shape, the trigger is more likely a property of *when* JUnit5's
interceptor chain executes relative to some CratonVM-internal timing/state
(e.g. after N test invocations, or specific to `@ExtendWith`/parameterized
lifecycle paths these classes share) than anything specific to the classes'
own test bodies.

Follow-on classloader, annotation-adapter, and reflection-identity residuals
found while retesting the named web-server class were fixed on 2026-07-18;
see [`mockwebenvironmentservletcomponentscanintegrationtests-hang-FIXED.md`](mockwebenvironmentservletcomponentscanintegrationtests-hang-FIXED.md).

## Merged 2026-07-17: absorbed `mockwebenvironmentservletcomponentscanintegrationtests-hang.md`

That doc was filed independently for the identical signature (retitled
mid-session to "cross-module cluster tracker" by a concurrent agent before
this merge) — folding its class list in here rather than maintaining two
docs for one bug. It added: `module/spring-boot-web-server`
`MockWebEnvironmentServletComponentScanIntegrationTests`;
`module/spring-boot-jdbc-test` `TestDatabaseAutoConfigurationNoEmbeddedTests`;
`module/spring-boot-flyway` `Flyway110AutoConfigurationTests`;
`module/spring-boot-batch-jdbc` `BatchJdbcAutoConfigurationWithoutJpaTests`;
`module/spring-boot-micrometer-tracing` `LogCorrelationEnvironmentPostProcessorTests`;
`module/spring-boot-hateoas` `HypermediaAutoConfigurationWithoutJacksonTests`;
`module/spring-boot-security` `PathRequestTests` + `SecurityFilterAutoConfigurationEarlyInitializationTests`
(both also carry `@ClassPathExclusions`); `module/spring-boot-liquibase`
`Liquibase423AutoConfigurationTests` (carries `@ClassPathOverrides`);
`module/spring-boot-r2dbc` `R2dbcAutoConfigurationTests` +
`R2dbcAutoConfigurationWithoutConnectionPoolTests`; `module/spring-boot-actuator`
`ThreadDumpEndpointTests`; `module/spring-boot-transaction`
`JtaAutoConfigurationTests`; `module/spring-boot-restclient-test`
`RestClientTestWithoutJacksonIntegrationTests`. That doc's own analysis
(reproduced above in this doc's "Update" sections in substance) found the
same thing this doc did: having a classpath-modification annotation neither
guarantees nor is required for the signature — see the bin5/bin11 update
sections above, which already incorporate that doc's findings.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.autoconfigure.DevToolsEmbeddedDataSourceAutoConfigurationTests` |
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.autoconfigure.DevToolsR2dbcAutoConfigurationTests` |
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.env.DevToolPropertiesIntegrationTests` |
| `module/spring-boot-servlet` | `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests` |
| `module/spring-boot-jsonb` | `org.springframework.boot.jsonb.autoconfigure.JsonbAutoConfigurationWithNoProviderTests` |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests` |
| `module/spring-boot-opentelemetry` | `org.springframework.boot.opentelemetry.autoconfigure.OpenTelemetryPropertiesTests` |
| `module/spring-boot-data-jpa` | `org.springframework.boot.data.jpa.autoconfigure.DataJpaRepositoriesAutoConfigurationTests` |
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests` |
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests` |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationJedisTests` (uses `@ClassPathExclusions`) |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests` (uses `@ClassPathExclusions`) |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.health.DataRedisHealthContributorAutoConfigurationTests` (uses `@ClassPathExclusions`) |
| `module/spring-boot-websocket` | `org.springframework.boot.websocket.autoconfigure.servlet.WebSocketMessagingAutoConfigurationTests` (uses `@ClassPathExclusions` on 2 test methods) |
| `module/spring-boot-web-server` | `MockWebEnvironmentServletComponentScanIntegrationTests` |
| `module/spring-boot-jdbc-test` | `TestDatabaseAutoConfigurationNoEmbeddedTests` |
| `module/spring-boot-flyway` | `Flyway110AutoConfigurationTests` |
| `module/spring-boot-batch-jdbc` | `BatchJdbcAutoConfigurationWithoutJpaTests` |
| `module/spring-boot-micrometer-tracing` | `LogCorrelationEnvironmentPostProcessorTests` |
| `module/spring-boot-hateoas` | `HypermediaAutoConfigurationWithoutJacksonTests` |
| `module/spring-boot-webclient-test` | `WebClientTestWithoutJacksonIntegrationTests` |
| `module/spring-boot-security` | `PathRequestTests` (uses `@ClassPathExclusions`) |
| `module/spring-boot-security` | `SecurityFilterAutoConfigurationEarlyInitializationTests` (uses `@ClassPathExclusions`) |
| `module/spring-boot-liquibase` | `Liquibase423AutoConfigurationTests` (uses `@ClassPathOverrides`) |
| `module/spring-boot-r2dbc` | `R2dbcAutoConfigurationTests` |
| `module/spring-boot-r2dbc` | `R2dbcAutoConfigurationWithoutConnectionPoolTests` |
| `module/spring-boot-actuator` | `ThreadDumpEndpointTests` |
| `module/spring-boot-transaction` | `JtaAutoConfigurationTests` |
| `module/spring-boot-restclient-test` | `RestClientTestWithoutJacksonIntegrationTests` |
