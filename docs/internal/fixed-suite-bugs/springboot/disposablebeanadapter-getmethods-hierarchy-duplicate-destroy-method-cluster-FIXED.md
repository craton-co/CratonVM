# Spring bean destroy-method resolution: `Class.getMethods()` returns duplicate entries for an overridden 0-arg method across the class/interface hierarchy, so `DisposableBeanAdapter` sees "N candidates" and refuses to pick one

**Status: FIXED — 2026-07-17.** Superseded by the corrected `Class.getMethods()` hierarchy merge in `native-builtins/src/lang_class.rs`. The collector now shadows same-signature declarations by specificity while preserving distinct overloads. See [`class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md`](class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md) for the regression and Spring Boot validation.

**Note — recurrence of a doc previously closed as RESOLVED.** This is the
same outer symptom (`BeanCreationException: ... Invalid destruction
signature`) as
[`../../internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`](../spring/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md),
closed 2026-07-12 with "the hypothesised reflection failure is not present
on current `dev`" based on a standalone probe of `DisposableBeanAdapter`
around **a plain bean with a single, non-overridden `AutoCloseable.close()`
implementation**. That probe shape does not exercise the bug found here
(see Root cause) — a bean whose destroy method is declared identically at
**more than one level of its class/interface hierarchy** (the overwhelming
common case for real destroy candidates: `close()`/`shutdown()`/`dispose()`
inherited from a JDK/library interface *and* overridden further down a
multi-level concrete class chain). The 2026-07-12 closure was too narrow to
catch this; the underlying mechanism was never fixed. This doc supersedes
that closure for this specific shape — the old doc's "no separate residual
was identified" conclusion is not accurate for the affected classes below.

## Symptom

| Module | Class | tests failed/total | Ambiguous bean : method | # candidates reported |
|---|---|---:|---|---:|
| `module/spring-boot-quartz` | `QuartzEndpointWebIntegrationTests` | 45/45 | `scheduler` : `shutdown` | 2 |
| `module/spring-boot-data-r2dbc` | `DataR2dbcAutoConfigurationTests` | 2/2 | `connectionFactory` : `dispose` | 2 |
| `module/spring-boot-data-r2dbc` | `DataR2dbcRepositoriesAutoConfigurationTests` | 4/5 | `connectionFactory` : `dispose` | 2 |
| `module/spring-boot-micrometer-tracing-brave` | `OtlpExemplarsAutoConfigurationTests` | 5/6 | `otlpMeterRegistry` : `close` | 3 |
| `module/spring-boot-data-r2dbc-test` | `DataR2dbcTestIntegrationTests` | 5/5 | `connectionFactory` : `dispose` | 2 |
| `module/spring-boot-data-r2dbc-test` | `DataR2dbcTestPropertiesIntegrationTests` | 2/2 | `connectionFactory` : `dispose` | 2 |
| `module/spring-boot-webmvc-test` | `WebMvcTestHtmlUnitWebDriverIntegrationTests` | 2/2 | `htmlUnitDriver` : `close` | 2 |

All four classes fail with the same wrapper text Spring uses for any
destroy-method resolution failure:

```
org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'connectionFactory' defined in org.springframework.boot.r2dbc.autoconfigure.ConnectionFactoryConfigurations$PoolConfiguration$PooledConnectionFactoryConfiguration: Invalid destruction signature
```

but the actual, specific cause (visible in the full `Caused by:` chain in
the `.out.log` — not in the `.err.log`, which is silent for these classes)
is:

```
Caused by: org.springframework.beans.factory.support.BeanDefinitionValidationException: Could not find unique destroy method on bean with name 'connectionFactory: Cannot resolve method 'dispose' to a unique method. Attempted to resolve to overloaded method with the least number of parameters but there were 2 candidates.
	at org.springframework.beans.factory.support.DisposableBeanAdapter.determineDestroyMethod(DisposableBeanAdapter.java:282)
	at org.springframework.beans.factory.support.DisposableBeanAdapter.<init>(DisposableBeanAdapter.java:129)
	at org.springframework.beans.factory.support.AbstractBeanFactory.registerDisposableBeanIfNecessary(AbstractBeanFactory.java:1929)
```

and for the Quartz/Micrometer cases:

```
Caused by: org.springframework.beans.factory.support.BeanDefinitionValidationException: Could not find unique destroy method on bean with name 'scheduler: Cannot resolve method 'shutdown' to a unique method. Attempted to resolve to overloaded method with the least number of parameters but there were 2 candidates.
Caused by: org.springframework.beans.factory.support.BeanDefinitionValidationException: Could not find unique destroy method on bean with name 'otlpMeterRegistry: Cannot resolve method 'close' to a unique method. Attempted to resolve to overloaded method with the least number of parameters but there were 3 candidates.
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-quartz.org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIn-e3de43e38499.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-r2dbc.org.springframework.boot.data.r2dbc.autoconfigure.DataR2dbcAutoC-f321f695d529.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-r2dbc.org.springframework.boot.data.r2dbc.autoconfigure.DataR2dbcRepos-4ea549d1613f.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-tracing-brave.org.springframework.boot.micrometer.tracing.brave.-b8682d5cc1c0.out.log`

**Addendum (same-day `bin13` triage batch):** three more classes hit the
identical `DisposableBeanAdapter.determineDestroyMethod` / "2 candidates"
shape and are folded into this doc rather than filed separately:
`module/spring-boot-data-r2dbc-test`'s `DataR2dbcTestIntegrationTests`
(5/5 failed, `connectionFactory` : `dispose`, full log
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-r2dbc-test.org.springframework.boot.data.r2dbc.test.autoconfigure.Data-97002775acaa.out.log`)
and `DataR2dbcTestPropertiesIntegrationTests` (2/2 failed, same bean/method,
full log
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-r2dbc-test.org.springframework.boot.data.r2dbc.test.autoconfigure.Data-2c501c185e19.out.log`)
— same `ExampleR2dbcApplication` context, same `io.r2dbc.pool.ConnectionPool.dispose()`
override shape as the sibling `spring-boot-data-r2dbc` module above, just
reached via the `@DataR2dbcTest` slice instead of `@SpringBootTest`. And
`module/spring-boot-webmvc-test`'s
`WebMvcTestHtmlUnitWebDriverIntegrationTests` (2/2 failed, `htmlUnitDriver` :
`close`, 2 candidates, full log
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc-test.org.springframework.boot.webmvc.test.autoconfigure.mockmvc.WebM-1614f7c92a30.out.log`)
— `org.openqa.selenium.htmlunit.HtmlUnitDriver.close()` is declared on the
`WebDriver` interface and overridden again down the `RemoteWebDriver`/
`HtmlUnitDriver` concrete-class chain, the same 2-level-override shape as
`ConnectionPool.dispose()`, confirming the bug isn't R2DBC/Quartz-specific
but any bean whose destroy method is re-declared at 2+ hierarchy levels.

**Second addendum (`bin12` triage batch, same day):** two more modules hit
the identical `DisposableBeanAdapter.determineDestroyMethod` / "N
candidates" shape, folded in here rather than filed separately:

- `module/spring-boot-hazelcast` — `hazelcastInstance` : `shutdown`, 2
  candidates (`com.hazelcast.core.HazelcastInstance` declares `shutdown()`,
  overridden again down the concrete `HazelcastInstanceProxy`/
  `HazelcastInstanceImpl` chain — the same shape as `ConnectionPool.dispose()`).
  Affects `HazelcastAutoConfigurationTests` (1/1),
  `HazelcastJpaDependencyAutoConfigurationTests` (4/4, also hits the
  `dataSource`/`shutdown` variant below on the same beans),
  `HazelcastHealthContributorAutoConfigurationIntegrationTests` (2/2),
  `HazelcastHealthContributorAutoConfigurationTests` (2/2), and
  `HazelcastHealthIndicatorTests` (2/4 — the other 2 tests pass). Full log:
  `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-hazelcast.org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoCon-768581b50666.out.log`.
- `module/spring-boot-data-rest` — `dataSource` (the embedded HSQLDB
  `DataSource`) : `shutdown`, 2 candidates. Affects
  `DataRestAutoConfigurationTests` (all 5 test-method failures in the run).
  Full log:
  `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-data-rest.org.springframework.boot.data.rest.autoconfigure.DataRestAutoConf-930875e17aa5.out.log`.

Both are the exact same mechanism as the R2DBC/Quartz/Micrometer cases above
— nothing Hazelcast- or DataSource-specific, further confirming this is a
generic `Class.getMethods()` defect, not tied to any one library.

## Root cause (grounded in source, not independently re-run/verified live)

Spring's `DisposableBeanAdapter.determineDestroyMethod` (via
`ClassUtils.getMethodIfAvailable` →
`findMethodWithMinimalParameters(Method[], String)`) walks **every**
`Method` returned by `bean.getClass().getMethods()`, filters to those
named e.g. `"close"`, and — among same-parameter-count matches — only
tolerates more than one if all-but-one `isBridge()`. If ≥2 *non-bridge*
`Method` objects share the name and 0-arg arity, Spring raises exactly the
`BeanDefinitionValidationException` seen above. On real HotSpot,
`Class.getMethods()` for a concrete class overriding an inherited
interface/superclass method returns **one** `Method` for that
name+descriptor (the most-derived declaration; HotSpot's
`Class.privateGetPublicMethods()` explicitly de-duplicates overridden
methods via its internal `MethodArray` merge, keeping only the
most-specific override).

CratonVM's implementation of `Class.getMethods()` does not do this
de-duplication. `native-builtins/src/lang_class.rs::collect_public_methods`
(the function that backs `native_class_get_methods`, lines 8522-8564):

```rust
fn collect_public_methods(...) -> ObjectRef {
    let mut metas: Vec<MethodMetadata> = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![class_id];

    while let Some(cid) = stack.pop() {
        if !visited.insert(cid) {
            continue;
        }
        let methods = declared_methods_with_synthetic(ctx, cid);
        for meta in methods {
            if meta.name == "<init>" || meta.name == "<clinit>" { continue; }
            if (meta.access_flags & 0x0001) != 0 {   // PUBLIC
                metas.push(meta);
            }
        }
        if !ctx.is_interface_class(cid) {
            if let Some(parent) = ctx.superclass_of(cid) { stack.push(parent); }
        }
        for iface_id in ctx.class_interfaces(cid) { stack.push(iface_id); }
    }
    build_mirror_array(ctx, metas.len(), |ctx, i| create_method_object(ctx, &metas[i]))
}
```

The `visited` `HashSet` only prevents **the same `class_id`** from being
walked twice (handles diamond interface re-inheritance); it does **not**
check whether a method with the same `(name, descriptor)` was already
pushed from a more-derived class earlier in the walk. So for a class
hierarchy where e.g. `PushMeterRegistry` (declares `close()`) extends
`MeterRegistry` (also declares/implements `close()` via `Closeable`), and
`OtlpMeterRegistry` further overrides `close()`, all three declarations
walk onto the stack (superclass chain + interfaces are pushed
unconditionally) and each contributes its own `MethodMetadata` for
`close()` to `metas` — three separate `Method` mirrors in the final array,
none marked as a bridge, so Spring's ambiguity counter reaches 3. The
2-candidate cases (`dispose()` on R2DBC `ConnectionFactory`/its connection
pool wrapper class, `shutdown()` on Quartz's `Scheduler` interface vs. its
`StdScheduler` implementation class) are the same mechanism with a
2-level-deep override instead of 3.

This is not confirmed via a live standalone repro this session (no
rebuild/run was performed, per this investigation's scope), but is a
direct reading of the function that implements `Class.getMethods()`, and
the "N candidates, N = number of hierarchy levels re-declaring the destroy
method name" pattern in the observed messages matches exactly what this
code would produce for each affected bean type. Confirming would mean:
build a small standalone probe calling `SomeConcreteClass.class.getMethods()`
for a class with a multi-level override of a given method name, print the
count and `getDeclaringClass()` of each returned `Method` named `X`, and
compare against the same probe on real JDK 25 (should show `1` there,
`N == override-chain length` under CratonVM).

**Update 2026-07-17 (bin11 rerun triage):** the identical signature recurs
against **6 more classes across 2 more modules** in the same rerun, all with
the same 2-candidate shape (a concrete class overriding a 0-arg method also
declared on an interface it implements one level up):

| Module | Class | tests failed/total | Ambiguous bean : method |
|---|---|---:|---|
| `module/spring-boot-r2dbc` | `R2dbcInitializationAutoConfigurationTests` | 7/8 | `connectionFactory` : `dispose` |
| `module/spring-boot-r2dbc` | `ConnectionFactoryHealthContributorAutoConfigurationTests` | 2/3 | `connectionFactory` : `dispose` |
| `module/spring-boot-r2dbc` | `ConnectionPoolMetricsAutoConfigurationTests` | 5/6 | `connectionFactory` : `dispose` |
| `module/spring-boot-actuator` | `ConfigurationPropertiesReportEndpointProxyTests` | 2/2 | `dataSource` : `shutdown` |

The r2dbc instances are `io.r2dbc.pool.ConnectionPool` — confirmed via
`javap` on `r2dbc-pool-1.0.2.RELEASE.jar`: `ConnectionPool` declares its own
concrete `public void dispose()` (`io.r2dbc.pool.ConnectionPool.class`)
while also `implements reactor.core.Disposable`, which separately declares
`public abstract void dispose()` (confirmed via `javap` on
`reactor-core-3.8.5.jar`) — exactly the 2-level-override shape this doc's
root cause describes: `collect_public_methods` walks both `ConnectionPool`
and `reactor/core/Disposable` (different `class_id`s, so the `visited`
dedup-by-class_id doesn't collapse them) and pushes a `dispose()`
`MethodMetadata` from each, giving `getMethods()` 2 non-bridge candidates
where HotSpot returns 1. The `spring-boot-actuator` instance
(`dataSource`/`shutdown`) is the same mechanism against
`EmbeddedDataSourceConfiguration`'s embedded-H2
`EmbeddedDatabaseFactory$EmbeddedDataSourceProxy`, matching the
`jooq-destroy-method-ambiguity-and-hang-FIXED.md` Cluster A shape exactly.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-r2dbc.org.springframework.boot.r2dbc.autoconfigure.R2dbcInitializationAutoC-8f8fc5f27d9f.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-r2dbc.org.springframework.boot.r2dbc.autoconfigure.health.ConnectionFactory-f0344e581e1d.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-r2dbc.org.springframework.boot.r2dbc.autoconfigure.metrics.ConnectionPoolMe-5dd240dd22f4.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-actuator.org.springframework.boot.actuate.context.properties.ConfigurationP-917a3d29882a.out.log`

## Suggested fix direction

`collect_public_methods` needs to de-duplicate by `(name, descriptor)`
across the whole walk, keeping only the entry from the **most-derived**
class (the first one encountered, since classes are pushed
depth-first-from-`class_id`-outward before their ancestors) — mirroring
`Class.getDeclaredMethods()`'s existing correct single-class behavior and
HotSpot's `MethodArray`-based merge. A `HashSet<(String, String)>` (or
similar) keyed on `(name, descriptor)`, checked before pushing a
`MethodMetadata` into `metas`, populated as classes closer to `class_id`
are visited first, would match JDK semantics. `collect_public_fields`
(same file, lines 8468-8502) does not have this specific failure mode
(field shadowing is legal and JDK's `getFields()` *does* return shadowed
fields from multiple levels), so it should not be touched by the same fix.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-quartz` | `org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests` |
| `module/spring-boot-data-r2dbc` | `org.springframework.boot.data.r2dbc.autoconfigure.DataR2dbcAutoConfigurationTests` |
| `module/spring-boot-data-r2dbc` | `org.springframework.boot.data.r2dbc.autoconfigure.DataR2dbcRepositoriesAutoConfigurationTests` |
| `module/spring-boot-micrometer-tracing-brave` | `org.springframework.boot.micrometer.tracing.brave.autoconfigure.OtlpExemplarsAutoConfigurationTests` |
| `module/spring-boot-data-r2dbc-test` | `org.springframework.boot.data.r2dbc.test.autoconfigure.DataR2dbcTestIntegrationTests` |
| `module/spring-boot-data-r2dbc-test` | `org.springframework.boot.data.r2dbc.test.autoconfigure.DataR2dbcTestPropertiesIntegrationTests` |
| `module/spring-boot-webmvc-test` | `org.springframework.boot.webmvc.test.autoconfigure.mockmvc.WebMvcTestHtmlUnitWebDriverIntegrationTests` |
| `module/spring-boot-r2dbc` | `org.springframework.boot.r2dbc.autoconfigure.R2dbcInitializationAutoConfigurationTests` (bin11, 7 of 8 failing test methods) |
| `module/spring-boot-r2dbc` | `org.springframework.boot.r2dbc.autoconfigure.health.ConnectionFactoryHealthContributorAutoConfigurationTests` (bin11, 2 of 3 failing test methods) |
| `module/spring-boot-r2dbc` | `org.springframework.boot.r2dbc.autoconfigure.metrics.ConnectionPoolMetricsAutoConfigurationTests` (bin11, 5 of 6 failing test methods) |
| `module/spring-boot-actuator` | `org.springframework.boot.actuate.context.properties.ConfigurationPropertiesReportEndpointProxyTests` (bin11, both failing test methods) |
| `module/spring-boot-hazelcast` | `org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoConfigurationTests` (bin12) |
| `module/spring-boot-hazelcast` | `org.springframework.boot.hazelcast.autoconfigure.HazelcastJpaDependencyAutoConfigurationTests` (bin12) |
| `module/spring-boot-hazelcast` | `org.springframework.boot.hazelcast.autoconfigure.health.HazelcastHealthContributorAutoConfigurationIntegrationTests` (bin12) |
| `module/spring-boot-hazelcast` | `org.springframework.boot.hazelcast.autoconfigure.health.HazelcastHealthContributorAutoConfigurationTests` (bin12) |
| `module/spring-boot-hazelcast` | `org.springframework.boot.hazelcast.health.HazelcastHealthIndicatorTests` (bin12, 2 of 4 failing test methods) |
| `module/spring-boot-data-rest` | `org.springframework.boot.data.rest.autoconfigure.DataRestAutoConfigurationTests` (bin12) |

The original 2026-07-12 doc lists ~34 classes across many more modules
(`spring-boot-micrometer-metrics`, `spring-boot-jdbc`, `spring-boot-hazelcast`,
`spring-boot-data-jpa`, `spring-boot-data-rest`, `spring-boot-jooq`,
`spring-boot-flyway`, `core/spring-boot-autoconfigure`) hitting the same
outer `BeanCreationException` wrapper — likely the same root cause, but
not re-verified here; only the 4 classes in this rerun's batch are
confirmed against the specific "N candidates" inner exception.
