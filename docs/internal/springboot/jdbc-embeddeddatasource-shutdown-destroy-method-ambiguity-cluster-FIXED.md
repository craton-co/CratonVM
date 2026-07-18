# `EmbeddedDataSourceConfiguration`'s `dataSource` bean fails destroy-method resolution: `shutdown()` has "2 candidates" at the same arg count

**Status: FIXED — 2026-07-17.** The shared `Class.getMethods()` override shadowing defect was corrected in `native-builtins/src/lang_class.rs`; distinct `shutdown` overloads remain visible, but duplicate declarations of the same signature no longer reach Spring's destroy-method resolver. See [`class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md`](class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md).

## Symptom

9 test methods across 2 classes fail identically, all rooted in the same
`dataSource` bean (`EmbeddedDataSourceConfiguration`) failing to construct
its `DisposableBeanAdapter`:

```
org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'dataSource' defined in org.springframework.boot.jdbc.autoconfigure.EmbeddedDataSourceConfiguration: Invalid destruction signature
     Caused by: org.springframework.beans.factory.support.BeanDefinitionValidationException: Could not find unique destroy method on bean with name 'dataSource: Cannot resolve method 'shutdown' to a unique method. Attempted to resolve to overloaded method with the least number of parameters but there were 2 candidates.
       org.springframework.beans.factory.support.DisposableBeanAdapter.determineDestroyMethod(DisposableBeanAdapter.java:282)
       org.springframework.beans.factory.support.DisposableBeanAdapter.<init>(DisposableBeanAdapter.java:129)
       org.springframework.beans.factory.support.AbstractBeanFactory.registerDisposableBeanIfNecessary(AbstractBeanFactory.java:1929)
```

| Class | Failing tests |
|---|---|
| `org.springframework.boot.jdbc.autoconfigure.EmbeddedDataSourceConfigurationTests` | `defaultEmbeddedDatabase`, `generateUniqueName` (2 of 2 — whole class) |
| `org.springframework.boot.jdbc.autoconfigure.health.DataSourceHealthContributorAutoConfigurationTests` | 7 of 14 tests, **all 7** carrying this exact same `Invalid destruction signature`/`shutdown` cause (confirmed by grepping the log: 14 occurrences of the two paired lines = 7 matched pairs = all 7 failures) |

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.EmbeddedDataSourceConfigurationTests.out.log`,
`.../module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.health.DataSourceHealthCon-4149ee2667b3.out.log`

**Update 2026-07-17 (bin2 rerun triage):** the identical `dataSource`/`shutdown`/
"2 candidates" signature also hits `module/spring-boot-data-jpa` — the same
`EmbeddedDataSourceConfiguration` bean, reached transitively via
`HibernateJpaConfiguration`'s constructor dependency on `dataSource`:

```
JUnit Jupiter:DataJpaRepositoriesWithEnversRevisionAutoConfigurationTests:testDefaultRepositoryConfiguration()
    => java.lang.AssertionError:
Expecting:
 <Unstarted application context ...[startupFailure=org.springframework.beans.factory.UnsatisfiedDependencyException]>
...
 org.springframework.beans.factory.UnsatisfiedDependencyException: Error creating bean with name 'org.springframework.boot.hibernate.autoconfigure.HibernateJpaConfiguration': Unsatisfied dependency expressed through constructor parameter 0: Error creating bean with name 'dataSource' defined in org.springframework.boot.jdbc.autoconfigure.EmbeddedDataSourceConfiguration: Invalid destruction signature
     Caused by: org.springframework.beans.factory.support.BeanDefinitionValidationException: Could not find unique destroy method on bean with name 'dataSource: Cannot resolve method 'shutdown' to a unique method. Attempted to resolve to overloaded method with the least number of parameters but there were 2 candidates.
```

All 9/9 tests in `DataJpaRepositoriesWithEnversRevisionAutoConfigurationTests`
fail this way (every test method independently builds a context that needs
`HibernateJpaConfiguration` → `dataSource`). Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-data-jpa.org.springframework.boot.data.jpa.autoconfigure.DataJpaRepositorie-8504008b77f6.out.log`

The sibling class `DataJpaRepositoriesAutoConfigurationTests` in the same
module (same rerun, same shard) **HANGs** rather than failing — its
`.out.log` is completely empty (0 bytes, not even the Spring Boot startup
banner) and its `.err.log` shows a steady sub-2s-interval repeating
`gen_heap::get_field: out-of-bounds field read dropped` WARN against
`org/junit/jupiter/engine/execution/InterceptingExecutableInvoker`
(`num_slots=0`), with no forward progress before the harness's timeout.
**This does not look like the same bug as the `dataSource`/`shutdown`
failure above** — the "zero JUnit output ever, steady sub-2s
`InterceptingExecutableInvoker` warning rhythm" shape exactly matches an
already-filed, unrelated cluster,
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md)
(a livelock that hangs *before* any `@Test`/`@BeforeEach` method ever runs,
across several other unrelated modules) — this class has been cross-referenced
there instead. Noted here only so a reader chasing the `dataSource` bug
doesn't also assume this HANG is the same root cause.

## Root cause (hypothesis, not confirmed against CratonVM's reflection natives)

Spring's `DisposableBeanAdapter.determineDestroyMethod`, when the destroy
method is *inferred* (Spring Boot's `EmbeddedDataSourceConfiguration` sets
`destroyMethod = "shutdown"`, a convention name it doesn't declare
explicitly), scans the bean class's methods for all overloads named
`shutdown`, then picks the one with the fewest parameters — throwing
`BeanDefinitionValidationException` only if **more than one** method ties
for that minimum. `EmbeddedDataSourceConfiguration`'s bean is Spring JDBC's
`EmbeddedDatabaseFactory$EmbeddedDataSourceProxy`, which declares exactly
one 0-arg `shutdown()` (from the `EmbeddedDatabase` interface) — on real
HotSpot (this test passes there) that resolves cleanly to one candidate.
CratonVM reports **two** candidates tied at the same (evidently 0) parameter
count, meaning its reflective method listing over this class (`Class.getMethods()`
or `ReflectionUtils.getUniqueDeclaredMethods`, whichever Spring's resolution
code uses here) returns a **duplicate** `Method` object for the same 0-arg
`shutdown()` — most likely once from the concrete class and once again from
the `EmbeddedDatabase` interface it implements (or a bridge method), where
real JDK reflection deduplicates an interface method against its overriding
class implementation and CratonVM's does not.

**Not root-caused to a specific `native-builtins` file:line in this pass**
— no `Class.getMethods`/`getDeclaredMethods`/method-dedup native source was
read to confirm this. This is the strongest hypothesis given the exact
error text ("2 candidates" at the "least number of parameters"), not a
verified finding.

**What would confirm/refute:** a standalone probe —
`Arrays.stream(EmbeddedDatabaseFactory.class.getDeclaredClasses())...` or
more directly, reflectively listing every `Method` named `shutdown` on a
constructed `EmbeddedDataSourceProxy` instance and comparing count/declaring-class
against real HotSpot (expect 1 there, CratonVM apparently reports 2).

## Discrepancy against `spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`

`docs/internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`
(dated 2026-07-12) closed a **34-class** cluster with this exact same outer
message (`"Invalid destruction signature"`) — including
`EmbeddedDataSourceConfiguration` by name in its own "affected modules"
list (`spring-boot-jdbc (EmbeddedDataSourceConfiguration)`) — as "RESOLVED,
stale current-dev report": a direct probe constructing a
`DisposableBeanAdapter` around a plain `AutoCloseable.close()` bean
succeeded, and the doc explicitly notes it never captured the actual inner
exception for the original 34-class report ("without a captured inner
exception or a distinct remaining failure mode").

**This rerun's failure is not the same mechanism that probe tested.** The
inner cause captured here is a **distinct, specific** one — `shutdown()`
resolving to 2 candidates at the same arg count — not a generic
interface-method-resolution failure on `AutoCloseable.close()`. The 2026-07-12
resolution's own probe only tested the `close()` case and explicitly said it
found "no separate residual ... to keep open" for lack of a captured inner
exception; this rerun supplies exactly the missing inner exception, and it
points at a narrower, still-live bug (method-listing duplication for one
specific overload set on one specific third-party class), not something the
2026-07-12 probe covered or ruled out. Filed as a new, distinct doc rather
than reopening/editing the RESOLVED one, per this session's instructions —
but flagging explicitly that the RESOLVED doc's "closed, no residual" claim
does not hold for this `shutdown()` shape.

## Affected classes

| Module | Class | Failing tests |
|---|---|---|
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.EmbeddedDataSourceConfigurationTests` | 2 of 2 |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.health.DataSourceHealthContributorAutoConfigurationTests` | 7 of 14 |
| `module/spring-boot-data-jpa` | `org.springframework.boot.data.jpa.autoconfigure.DataJpaRepositoriesWithEnversRevisionAutoConfigurationTests` | 9 of 9 |
