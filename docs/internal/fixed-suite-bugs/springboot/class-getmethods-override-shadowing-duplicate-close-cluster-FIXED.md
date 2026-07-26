# `Class.getMethods()` doesn't shadow overridden methods — Spring's destroy-method resolution sees N duplicate `close()` candidates

**Status: FIXED — 2026-07-17.** The public-method collector now merges a method by its name and full descriptor while walking the class/interface hierarchy, retaining the most-specific declaration. The regression test covers class and interface overrides plus genuine overloads. A Java 25 Spring Boot probe passed `CompositeMeterRegistryAutoConfigurationTests` (4/4) in JIT and `--nojit` modes. The separately discovered Redis listener-factory residual was also fixed: the compatibility native delegates non-null factories to the real setter and keeps its narrow null bootstrap escape only; `DataRedisAutoConfigurationTests` then passed 56/56 in both modes.

## Symptom

Module `module/spring-boot-micrometer-metrics`, 18 classes fully affected (every
failure in the class is this signature) plus 1 partially affected (1 of 4
failures; the other 3 are a different bug, see
[`invokespecial-lambda-super-reference-retarget-selfrecursion-stackoverflow-FIXED.md`](invokespecial-lambda-super-reference-retarget-selfrecursion-stackoverflow-FIXED.md)):

| Class | Failures from this cause |
|---|---:|
| `CompositeMeterRegistryAutoConfigurationTests` | 2/2 |
| `MeterRegistryCustomizerTests` | 3/3 |
| `MetricsAutoConfigurationIntegrationTests` | 6/6 |
| `MetricsAutoConfigurationMeterRegistryPostProcessorIntegrationTests` | 3/3 |
| `export.appoptics.AppOpticsMetricsExportAutoConfigurationTests` | 4/4 |
| `export.atlas.AtlasMetricsExportAutoConfigurationTests` | 4/4 |
| `export.datadog.DatadogMetricsExportAutoConfigurationTests` | 4/4 |
| `export.dynatrace.DynatraceMetricsExportAutoConfigurationTests` | 5/5 |
| `export.elastic.ElasticMetricsExportAutoConfigurationTests` | 4/4 |
| `export.ganglia.GangliaMetricsExportAutoConfigurationTests` | 4/4 |
| `export.graphite.GraphiteMetricsExportAutoConfigurationTests` | 6/6 |
| `export.humio.HumioMetricsExportAutoConfigurationTests` | 4/4 |
| `export.influx.InfluxMetricsExportAutoConfigurationTests` | 1/4 (partial — see above) |
| `export.jmx.JmxMetricsExportAutoConfigurationTests` | 4/4 |
| `export.kairos.KairosMetricsExportAutoConfigurationTests` | 4/4 |
| `export.otlp.OtlpMetricsExportAutoConfigurationTests` | 17/17 |
| `export.stackdriver.StackdriverMetricsExportAutoConfigurationTests` | 4/4 |
| `export.statsd.StatsdMetricsExportAutoConfigurationTests` | 4/4 |

Representative trace (`CompositeMeterRegistryAutoConfigurationTests`, full log
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-889b5cc36d7e.out.log`):

```
=> java.lang.AssertionError:
Expecting:
 <Unstarted application context ...[startupFailure=org.springframework.beans.factory.BeanCreationException]>
to have a single bean of type:
 <io.micrometer.core.instrument.MeterRegistry>:
but context failed to start:
 org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'noOpMeterRegistry' defined in ...NoOpMeterRegistryConfiguration: Invalid destruction signature
   ...
 Caused by: org.springframework.beans.factory.support.BeanDefinitionValidationException: Could not find unique destroy method on bean with name 'noOpMeterRegistry: Cannot resolve method 'close' to a unique method. Attempted to resolve to overloaded method with the least number of parameters but there were 2 candidates.
   at org.springframework.beans.factory.support.DisposableBeanAdapter.determineDestroyMethod(DisposableBeanAdapter.java:282)
```

The "N candidates" count varies by bean type/class-hierarchy depth — 2 for
`CompositeMeterRegistry`/`AtlasMeterRegistry`/`GraphiteMeterRegistry`, 3 for
`AppOpticsMeterRegistry`/`OtlpMeterRegistry` — always exactly matching the
number of classes in that bean's hierarchy (from `MeterRegistry` down) that
each declare their own `close()` override.

## Root cause (CONFIRMED — file:line)

`org.springframework.beans.factory.support.DisposableBeanAdapter.determineDestroyMethod`
resolves an inferred (`close()`/`shutdown()`) destroy method by calling
`Class.getMethods()` (confirmed via `javap -c` on the real
`spring-beans-7.0.7.jar`) and picking the unique method named `close` with the
fewest parameters. Real HotSpot's `Class.getMethods()` returns exactly ONE
entry per (name, descriptor) — an overriding method in a subclass shadows the
overridden method in its superclass, per `Class.privateGetPublicMethods()`'s
documented merge behavior.

CratonVM's implementation does not perform that merge. In
`native-builtins/src/lang_class.rs`:

- `native_class_get_methods` (line 8682) calls `collect_public_methods(ctx, class_id)` (line 8718).
- `collect_public_methods` (lines 8522–8564) walks every class in the
  hierarchy (superclass chain + interfaces) via a `visited: HashSet<ClassId>`
  that dedups **by class**, then for each visited class pushes every public,
  non-`<init>`/`<clinit>` method straight into a flat `metas: Vec<MethodMetadata>`
  (lines 8538–8546) — one push per class per matching method, with **no
  comparison against methods already collected from a more-derived class**.
  The doc comment at lines 8504–8521 correctly cites JDK 25's
  `Class.privateGetPublicMethods()` as the semantic reference (and replicates
  its interface-vs-class superclass-skip nuance) but never implements the
  override-shadowing merge step that method actually performs.

For a concrete example: `io.micrometer.core.instrument.composite.CompositeMeterRegistry`
(bean `compositeMeterRegistry`/`noOpMeterRegistry`) is a subclass of the
abstract class `io.micrometer.core.instrument.MeterRegistry` and overrides
`public void close()` (confirmed via `javap -c` on the real
`micrometer-core-1.17.0-RC1.jar`: `CompositeMeterRegistry.close()` calls
`invokespecial MeterRegistry.close()` internally — a normal override, not an
overload). `collect_public_methods` pushes `close()` once when it visits
`CompositeMeterRegistry` and again when it visits `MeterRegistry`, so the
returned `Method[]` contains 2 entries named `close` with identical `()V`
descriptors — which is exactly the "2 candidates" Spring's
`findMethodWithMinimalParameters`-style resolution then rejects. Bean types
with a deeper override chain (e.g. `StepMeterRegistry` → `PushMeterRegistry`
→ `MeterRegistry`, each overriding `close()`) produce 3 candidates, matching
the observed variance.

`collect_public_fields` (same file, ~lines 8470–8502) has the identical
flat-push-with-no-signature-dedup pattern for field hiding, though that's
less commonly hit in practice than method overriding.

No existing test (`vm/tests/t13_class_conformance.rs`) or known-issues doc
currently covers this specific override-dedup gap; `docs/internal/spring-boot-probe-sweep/SBR-05-getdeclaredmethods-order.md`
covers only declaration *order*, not duplicate overridden entries.

## Update 2026-07-17 (bin7 rerun triage) — same bug via an explicit `@Bean(destroyMethod="shutdown")`, not just `close()`, and via an interface+concrete override, not just concrete-subclass-over-abstract-superclass

Independently found and root-caused (before discovering this doc) the exact
same `collect_public_methods` gap via a different bean/method-name shape in
`module/spring-boot-data-redis` — confirming this isn't `close()`-specific
or `MeterRegistry`-hierarchy-specific:

| Class | Failing tests |
|---|---:|
| `DataRedisAutoConfigurationTests` | 55 of 56 |
| `DataRedisReactiveAutoConfigurationTests` | all |
| `DataRedisReactiveHealthContributorAutoConfigurationTests` | 3 of 3 |
| `observation.LettuceObservationAutoConfigurationTests` | 2 of 2 |

```
Caused by: org.springframework.beans.factory.support.BeanDefinitionValidationException: Could not find unique destroy method
  on bean with name 'lettuceClientResources: Cannot resolve method 'shutdown' to a unique method. Attempted to resolve to
  overloaded method with the least number of parameters but there were 2 candidates.
	at org.springframework.beans.factory.support.DisposableBeanAdapter.determineDestroyMethod(DisposableBeanAdapter.java:282)
```
Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-redis.org.springframework.boot.data.redis.autoconfigure.DataRedisAutoC-56e031b16c90.out.log`,
`...DataRedisReact-881823a398e5.out.log`, `...health.DataRed-e81c36b7bbb3.out.log`,
`shard3/logs/module_spring-boot-data-redis.org.springframework.boot.data.redis.autoconfigure.observation.Le-015484ac115d.out.log`

Two things this occurrence adds to the confirmed mechanism above:

1. **Explicit `@Bean(destroyMethod = "shutdown")`, not JDK-inferred `close()`/`shutdown()`.**
   `LettuceConnectionConfiguration` declares `@Bean(destroyMethod =
   "shutdown")` for `lettuceClientResources`
   (`io.lettuce.core.resource.DefaultClientResources`), so this goes through
   `DisposableBeanAdapter.determineDestroyMethod`'s **named-method**
   resolution path (`BeanUtils.findMethodWithMinimalParameters`), a
   different call site from the `MeterRegistry` cluster's
   JDK-`AutoCloseable`-inference path — same underlying `getMethods()` bug,
   reached from two different Spring entry points.
2. **The duplication source is an interface's abstract method vs. the
   concrete class's override, not two concrete classes in a superclass
   chain.** `DefaultClientResources implements ClientResources`; both
   declare `shutdown()` (0-arg) and `shutdown(long,long,TimeUnit)` (3-arg)
   — verified via `javap` on `lettuce-core-7.5.1.RELEASE.jar`. Real
   HotSpot's `getMethods()` returns the concrete override only; CratonVM's
   `collect_public_methods` walks `class_interfaces(cid)` (line ~8555 in
   this doc's cited version of `lang_class.rs`) exactly like it walks the
   superclass chain, with the same missing shadowing-merge step, so it
   double-counts here too — confirming the bug isn't narrowly about
   `superclass_of` walks, `ctx.class_interfaces` hits it identically.

Since `shutdown` has 2 real overloads (0-arg and 3-arg) here, un-deduped
`getMethods()` returns 4 `Method` objects total (2 signatures × 2 declaring
locations), and the minimal-parameter-count tie is between the 2 duplicate
0-arg entries — matching the observed "2 candidates" exactly.

## Distinct, lower-confidence residual found alongside these classes

`DataRedisAnnotationDrivenConfigurationTests` (2 of 5 tests FAIL in the
same module) is **not** part of this cluster — it never touches
`lettuceClientResources` (no mention in its stack trace at all). Its 2
failing tests (`containerConfigurationMatchesDefaults`,
`containerCanBeConfigured`) both fail with `IllegalArgumentException:
RedisConnectionFactory is not set` at
`RedisMessageListenerContainer.afterPropertiesSet`, even though the test's
own `TestConfiguration` explicitly registers a `@Bean RedisConnectionFactory
redisConnectionFactory() { return mock(RedisConnectionFactory.class); }`.
The other 3 tests in the same class (including
`registersContainerAndAnnotationProcessor`, using the exact same bean
setup) pass. Not root-caused — flagged as a possible Mockito-mock
reliability flake (a fresh `mock()` is constructed per-test inside the
`@Bean` method) rather than guessed at further; left out of the "Affected
classes" table below.

## Discrepancy with a prior RESOLVED doc

`../spring/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`
(dated 2026-07-12) closed an earlier 34-class "Invalid destruction signature"
cluster after a standalone probe against a single concrete `AutoCloseable`
bean succeeded, and explicitly noted it never captured the actual inner
exception CratonVM was throwing under the generic wrapper message. That
probe's bean shape (one class, no override chain) doesn't reproduce this bug
— the defect only manifests when the destroy method is declared on **more
than one class in the hierarchy** (an override, not a single declaration).
This session captured the actual inner exception
(`BeanDefinitionValidationException: ... 2/3 candidates`) for the first time
and traced it to a confirmed, distinct root cause in `getMethods()`. This is
not a re-occurrence of the previously-closed bug; it's the "no captured inner
exception" residual that RESOLVED doc's own "Next step" section asked for,
now filled in. Filing as a new, separate OPEN doc rather than reopening the
old one, per this session's triage instructions.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.CompositeMeterRegistryAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.MeterRegistryCustomizerTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.MetricsAutoConfigurationIntegrationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.MetricsAutoConfigurationMeterRegistryPostProcessorIntegrationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.appoptics.AppOpticsMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.atlas.AtlasMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.datadog.DatadogMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.dynatrace.DynatraceMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.elastic.ElasticMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.ganglia.GangliaMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.graphite.GraphiteMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.humio.HumioMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.influx.InfluxMetricsExportAutoConfigurationTests` (partial — 1 of 4 failures) |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.jmx.JmxMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.kairos.KairosMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp.OtlpMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.stackdriver.StackdriverMetricsExportAutoConfigurationTests` |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.statsd.StatsdMetricsExportAutoConfigurationTests` |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationTests` (55/56, added bin7) |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.DataRedisReactiveAutoConfigurationTests` (added bin7) |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.health.DataRedisReactiveHealthContributorAutoConfigurationTests` (added bin7) |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.observation.LettuceObservationAutoConfigurationTests` (added bin7) |
