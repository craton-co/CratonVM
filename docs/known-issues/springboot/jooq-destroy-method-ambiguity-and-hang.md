# jOOQ/H2Console autoconfigure — destroy-method ambiguity (residual of a RESOLVED doc) + unconfirmed HANG

**Status: OPEN — found 2026-07-17**

## Cluster A — duplicate 'shutdown' destroy-method candidates (spans `module/spring-boot-jooq`, `module/spring-boot-h2console`, `module/spring-boot-jdbc-test`, `module/spring-boot-flyway`, `module/spring-boot-batch-jdbc`, `module/spring-boot-integration`)

Same exact signature confirmed in **six separate modules** in this rerun —
folded into one shared cluster per this investigation's clustering
convention rather than filing a duplicate doc:

- `module/spring-boot-jooq` | `JooqFlywayDatabaseInitializationTests` — 3/3 failures
- `module/spring-boot-h2console` | `H2ConsoleAutoConfigurationTests` — 2/4 failures (`singleDataSourceUrlIsLoggedWhenOnlyOneAvailable`, `dataSourceIsNotInitializedEarly`; the other 2 failures in this class are an unrelated CapturedOutput-logging issue, folded into [`conditionevaluationreport-capturedoutput-empty-cluster.md`](conditionevaluationreport-capturedoutput-empty-cluster.md))
- `module/spring-boot-jdbc-test` | `AutoConfigureTestDatabaseWithMultipleDatasourcesIntegrationTests` — 1/1 (bean `secondaryDataSource`)
- `module/spring-boot-jdbc-test` | `JdbcTestWithAutoConfigureTestDatabaseReplaceAutoConfiguredIntegrationTests` — 1/1
- `module/spring-boot-jdbc-test` | `JdbcTestWithAutoConfigureTestDatabaseReplaceAutoConfiguredWithoutOverrideIntegrationTests` — 1/1
- `module/spring-boot-jdbc-test` | `JdbcTestWithAutoConfigureTestDatabaseReplaceNoneIntegrationTests` — 1/1
- `module/spring-boot-jdbc-test` | `JdbcTestWithAutoConfigureTestDatabaseReplacePropertyAutoConfiguredIntegrationTests` — 1/1
- `module/spring-boot-jdbc-test` | `JdbcTestWithAutoConfigureTestDatabaseReplacePropertyNoneIntegrationTests` — 1/1
- `module/spring-boot-jdbc-test` | `TestDatabaseAutoConfigurationTests` — 1/3 (`whenUsingAotGeneratedArtifactsEmbeddedDataSourceFactoryBeanIsNotDefined`)
- `module/spring-boot-flyway` | `FlywayEndpointTests` — 2/2
- `module/spring-boot-flyway` | `FlywayAutoConfigurationTests` — 61/61 (entire class; the whole 180s run is this one signature repeated, not a mix of causes — confirmed via `grep -c "Cannot resolve method 'shutdown'"` == 61 == failed count). Note: this is a **different, unrelated symptom** from the already-`FIXED` `flyway-cglib-heap-corruption-sigsegv-crash` doc for the same class (that one was a fatal SIGSEGV; this is a clean FAIL) — not a discrepancy, just two distinct bugs sharing a class.
- `module/spring-boot-batch-jdbc` | `BatchJdbcAutoConfigurationTests` — 24/26 (the other 2 are unrelated, see `batch-jdbc-mergedannotation-isdirectlypresent-abstractmethoderror.md`)
- `module/spring-boot-integration` | `IntegrationAutoConfigurationTests` — 4/9 (`whenIntegrationJdbcDataSourceInitializerIsEnabledThenFlywayCanBeUsed`, `integrationJdbcDataSourceInitializerEnabledByDefaultWithEmbeddedDb`, `integrationJdbcDataSourceInitializerEnabled`, `integrationJdbcDataSourceInitializerDisabled`; the other 5 are unrelated, see `integration-mbeanserver-getdomains-missing-native-abstractmethoderror.md`)

Every one of the new instances above is, byte-for-byte, the same
`Caused by: ... Cannot resolve method 'shutdown' to a unique method ...
2 candidates` inner exception under `DisposableBeanAdapter.determineDestroyMethod`,
against the same `dataSource`/`secondaryDataSource` bean type
(`org.springframework.boot.jdbc.autoconfigure.EmbeddedDataSourceConfiguration`
or `org.springframework.boot.jdbc.test.autoconfigure.TestDatabaseAutoConfiguration`
— both produce Spring's embedded-H2 `EmbeddedDatabaseFactory$EmbeddedDataSourceProxy`
wrapping `EmbeddedDatabase`, which declares the single no-arg `shutdown()`
method this whole cluster trips on). The breadth (6 modules, ~13 classes, 90+
individual test methods, every occurrence pinned to the identical bean type)
strongly corroborates the existing hypothesis below over any per-module
explanation.

All instances are the same shape:

```
Caused by: org.springframework.beans.factory.support.BeanDefinitionValidationException: Could not find unique destroy method on bean with name 'dataSource: Cannot resolve method 'shutdown' to a unique method. Attempted to resolve to overloaded method with the least number of parameters but there were 2 candidates.
   at org.springframework.beans.factory.support.DisposableBeanAdapter.determineDestroyMethod(DisposableBeanAdapter.java:282)
```

(bean `'dataSource'` from `org.springframework.boot.jdbc.autoconfigure.EmbeddedDataSourceConfiguration`, or from an equivalent `EarlyInitializationConfiguration`/`H2ConsoleAutoConfiguration$H2ConsoleLogger` factory in the h2console case — same embedded-`DataSource` destroy-method resolution path either way)

Logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-jooq.org.springframework.boot.jooq.autoconfigure.JooqFlywayDatabaseInitializationTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-h2console.org.springframework.boot.h2console.autoconfigure.H2ConsoleAutoCon-3c0cb3295e8f.out.log`

### Relationship to an existing RESOLVED doc — discrepancy

[`../../internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`](../../internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md)
covers a **generic outer** `BeanCreationException("Invalid destruction
signature")` wrapper with **no captured inner exception**, closed RESOLVED
2026-07-12 via a synthetic `AutoCloseable` probe. This 2026-07-17 failure has
a **specific inner exception that doc never captured**: Spring's
`determineDestroyMethod` finds 2 overloaded `'shutdown'` methods with equal
minimal parameter count and can't disambiguate — reading as CratonVM
reflection (`Class.getMethods()`) surfacing a duplicate/ambiguous `shutdown`
candidate (e.g. a bridge-method duplicate, or both an interface and impl
method surfacing separately where HotSpot would collapse them).

**This is plausibly the exact inner cause the RESOLVED doc's own "next
step" was still looking for, not a new unrelated bug.** Recommend treating
as a residual/reopened issue in the same family rather than a closed
matter. Not pinned to an exact reflection file:line.

### Root cause (Cluster A)

Hypothesis: CratonVM's `Class.getMethods()`/`getDeclaredMethods()` for the
embedded `DataSource` type returns a `shutdown` method twice (once via an
interface, once via the concrete class, or a synthetic bridge method) where
real HotSpot returns one — causing Spring's least-params-wins destroy-method
disambiguation to see a tie. Unconfirmed.

**Cross-reference (2026-07-17, same-day parallel triage):** this hypothesis
is confirmed at the source level (though not by a live rebuild+repro) in
[`disposablebeanadapter-getmethods-hierarchy-duplicate-destroy-method-cluster.md`](disposablebeanadapter-getmethods-hierarchy-duplicate-destroy-method-cluster.md),
found independently against `module/spring-boot-quartz`,
`module/spring-boot-data-r2dbc`, and `module/spring-boot-micrometer-tracing-brave`
in the same rerun (`Cannot resolve method 'shutdown'/'dispose'/'close' to a
unique method` with 2-3 candidates each). That doc pins the exact gap to
`native-builtins/src/lang_class.rs::collect_public_methods` (backing
`Class.getMethods()`): it walks the full class+interface hierarchy and
pushes every declaring class's copy of a same-named/same-descriptor public
method into the result array, deduplicating only by `class_id` (so a
diamond re-visit of the same interface is skipped) but **not** by
`(name, descriptor)` across different classes in the hierarchy — unlike
real HotSpot's `Class.privateGetPublicMethods()`, which collapses an
overridden method down to its single most-derived declaration. This
directly explains "N candidates" for any destroy-method name declared at
N levels of a bean's class/interface chain, `dataSource`/`shutdown`
included.

### Update 2026-07-17 (bin5 rerun triage) — 2 more classes, 1 more module (`spring-boot-liquibase`)

Same exact signature, confirmed via the `.out.log`'s `Caused by:` chain
(not the `.err.log`, silent for these too):

- `module/spring-boot-liquibase` | `LiquibaseEndpointTests` — 5/9 failures.
  All 5 are, byte-for-byte, `Caused by: ...
  BeanDefinitionValidationException: Could not find unique destroy method
  on bean with name 'dataSource: Cannot resolve method 'shutdown' to a
  unique method. Attempted to resolve to overloaded method with the least
  number of parameters but there were 2 candidates.` — bean `dataSource`
  from an inline `DataSourceWithSchemaConfiguration`/
  `MultipleDataSourceLiquibaseConfiguration` test-fixture `@Configuration`,
  same embedded HSQLDB `EmbeddedDatabaseFactory$EmbeddedDataSourceProxy`
  wrapping `EmbeddedDatabase` shape as the rest of this cluster.
  Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-liquibase.org.springframework.boot.liquibase.actuate.endpoint.LiquibaseEndpointTests.out.log`
- `module/spring-boot-liquibase` | `LiquibaseAutoConfigurationTests` — at
  least 9 of its failures (the class has more failures than this alone;
  not all re-examined) are the identical
  `EmbeddedDataSourceConfiguration`/`dataSource`/`shutdown`/2-candidates
  shape:
  ```
  org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'dataSource' defined in org.springframework.boot.jdbc.autoconfigure.EmbeddedDataSourceConfiguration: Invalid destruction signature
  ...
  Caused by: org.springframework.beans.factory.support.BeanDefinitionValidationException: Could not find unique destroy method on bean with name 'dataSource: Cannot resolve method 'shutdown' to a unique method. Attempted to resolve to overloaded method with the least number of parameters but there were 2 candidates.
  ```
  This is the **same `EmbeddedDataSourceConfiguration` class** already
  named as affected in the original 2026-07-12
  `spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`
  doc's module list (`spring-boot-jdbc (EmbeddedDataSourceConfiguration)`)
  — i.e. this is the same discrepancy-with-a-RESOLVED-doc situation as
  `taskscheduling-invalid-destruction-signature-recurrence.md` describes
  for `TaskSchedulingAutoConfigurationTests`, for the *other* bean type
  that RESOLVED doc named. The 2026-07-12 closure's standalone probe (a
  plain `AutoCloseable` bean) never exercised this specific
  `EmbeddedDataSourceConfiguration`/HSQLDB-`shutdown` shape either.
  Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-liquibase.org.springframework.boot.liquibase.autoconfigure.LiquibaseAutoCon-2be69b0874d5.out.log`

Both add to `collect_public_methods`'s confirmed root cause above (same
`dataSource`/`shutdown`/2-candidates fingerprint already pinned for
`spring-boot-jooq`/`spring-boot-h2console`/etc.) — no new mechanism, just
2 more confirmed instances in a 7th and 8th affected module.

## Cluster B — `JooqAutoConfigurationTests`: HANG after JAXB ContextFactory load, unconfirmed

`.out.log` is empty. `.err.log` (522 lines, ~292KB) stops silently after:

```
FINE [jakarta.xml.bind] Checking system property jakarta.xml.bind.JAXBContextFactory
FINE [jakarta.xml.bind]   not found
FINE [jakarta.xml.bind] ServiceProvider loading Facility used; returning object [org.glassfish.jaxb.runtime.v2.JAXBContextFactory]
FINE [org.glassfish.jaxb.runtime.v2.ContextFactory] Property org.glassfish.jaxb.XmlAccessorFactory is not active. Using JAXB's implementation.
```

Last timestamped WARN at 20:16:14.7Z; log file closes ~20:21:02Z (~5min
silent gap before being killed) — consistent with a genuine hang, but **no
stack-dump-on-timeout was captured**, so there's no Java-side thread stack
to confirm where it's actually stuck.

Two shape-similar but **not-matching** docs exist for a different suite
(already resolved weeks ago, not this bug):
`docs/internal/fixed-suite-bugs/hibernate-jaxb-classload-synthetic-stub-rescan-storm.md`
(FIXED 2026-06-18, Hibernate suite, classpath-rescan storm) and
`docs/internal/hibernate-bugs/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md`
(RESOLVED/does-not-reproduce 2026-06-20, also Hibernate). No Spring
Boot/jOOQ-specific JAXB-hang doc exists.

### Root cause (Cluster B)

**Not confirmed — no stack trace evidence.** Strongest hypothesis is a
JAXB/reflection class-model-building stall in the same general family as
the (unrelated-suite) Hibernate docs above, but this is unattributed. Needs
a live repro with a stack dump before assigning root cause.

**Update 2026-07-17 (bin6 rerun triage):** Cluster B's HANG signature
(`InterceptingExecutableInvoker`/`InvocationInterceptorChain` `gc::guard`
OOB-field-read warnings repeating forever, no test progress) recurs
identically across **5 more HANG classes in 5 more modules** in this same
rerun, none of which touch JAXB — see
[`mockwebenvironmentservletcomponentscanintegrationtests-hang.md`](mockwebenvironmentservletcomponentscanintegrationtests-hang.md),
now generalized into a cross-module cluster covering
`spring-boot-jdbc-test`, `spring-boot-flyway`, `spring-boot-batch-jdbc`,
`spring-boot-micrometer-tracing`, and `spring-boot-hateoas`. The breadth
argues against a JAXB-specific mechanism for Cluster B here too — treat the
"stops right after JAXB `ContextFactory` load" detail as this instance's
last-log-line-before-going-silent, not necessarily its cause, and see that
doc for the generalized (still unconfirmed) hypothesis.

## Update 2026-07-17 (bin8 rerun triage) — 2 more classes, 1 more module (`spring-boot-jooq-test`)

Same exact signature confirmed via the `.out.log`'s `Caused by:` chain
(`.err.log` silent for both, same as every other instance in this cluster):

- `module/spring-boot-jooq-test` | `JooqTestIntegrationTests` — all 7 test
  methods fail; each shows `IllegalStateException: ApplicationContext
  failure threshold (1) exceeded: skipping repeated attempt to load context`
  wrapping the same root `BeanDefinitionValidationException` for the
  `ExampleJooqApplication` context — i.e. the underlying `dataSource`/
  `shutdown`/2-candidates failure happens once (on the first test method)
  and every subsequent method in the class short-circuits on Spring's
  context-failure-threshold cache rather than re-attempting.
- `module/spring-boot-jooq-test` | `JooqTestPropertiesIntegrationTests` — 2/2
  failures, same shape: one test hits the root cause directly —
  ```
  Caused by: org.springframework.beans.factory.support.BeanDefinitionValidationException: Could not find unique destroy method on bean with name 'dataSource: Cannot resolve method 'shutdown' to a unique method. Attempted to resolve to overloaded method with the least number of parameters but there were 2 candidates.
  ```
  — and the nested `NestedTests.propertiesFromEnclosingClassAffectNestedTests`
  hits the same context-failure-threshold short-circuit as
  `JooqTestIntegrationTests` above.

Both use `@JooqTest`'s auto-configured embedded test `DataSource`
(`ExampleJooqApplication`), the same `EmbeddedDataSourceConfiguration`/
`EmbeddedDatabaseFactory$EmbeddedDataSourceProxy` shape as every other
instance in Cluster A — no new mechanism, just 2 more confirmed instances in
a 9th affected module (and the first instance in this cluster showing the
"first test poisons the whole class via context-failure-threshold" secondary
effect this explicitly, though it's implicit in `FlywayAutoConfigurationTests`'s
61/61 note above too).

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-jooq-test.org.springframework.boot.jooq.test.autoconfigure.JooqTestIntegrationTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-jooq-test.org.springframework.boot.jooq.test.autoconfigure.JooqTestProperti-12a195b471ea.out.log`

## Affected classes

- `module/spring-boot-jooq-test` | `JooqTestIntegrationTests` (Cluster A, added bin8, all 7 failures)
- `module/spring-boot-jooq-test` | `JooqTestPropertiesIntegrationTests` (Cluster A, added bin8, both failures)
- `module/spring-boot-jooq` | `JooqFlywayDatabaseInitializationTests` (Cluster A)
- `module/spring-boot-h2console` | `H2ConsoleAutoConfigurationTests` (Cluster A, 2 of its 4 failing test methods only)
- `module/spring-boot-jdbc-test` | `AutoConfigureTestDatabaseWithMultipleDatasourcesIntegrationTests` (Cluster A)
- `module/spring-boot-jdbc-test` | `JdbcTestWithAutoConfigureTestDatabaseReplaceAutoConfiguredIntegrationTests` (Cluster A)
- `module/spring-boot-jdbc-test` | `JdbcTestWithAutoConfigureTestDatabaseReplaceAutoConfiguredWithoutOverrideIntegrationTests` (Cluster A)
- `module/spring-boot-jdbc-test` | `JdbcTestWithAutoConfigureTestDatabaseReplaceNoneIntegrationTests` (Cluster A)
- `module/spring-boot-jdbc-test` | `JdbcTestWithAutoConfigureTestDatabaseReplacePropertyAutoConfiguredIntegrationTests` (Cluster A)
- `module/spring-boot-jdbc-test` | `JdbcTestWithAutoConfigureTestDatabaseReplacePropertyNoneIntegrationTests` (Cluster A)
- `module/spring-boot-jdbc-test` | `TestDatabaseAutoConfigurationTests` (Cluster A, 1 of 3 failing test methods)
- `module/spring-boot-flyway` | `FlywayEndpointTests` (Cluster A)
- `module/spring-boot-flyway` | `FlywayAutoConfigurationTests` (Cluster A, all 61 failures)
- `module/spring-boot-batch-jdbc` | `BatchJdbcAutoConfigurationTests` (Cluster A, 24 of 26 failures)
- `module/spring-boot-integration` | `IntegrationAutoConfigurationTests` (Cluster A, 4 of 9 failures)
- `module/spring-boot-jooq` | `JooqAutoConfigurationTests` (Cluster B, HANG)
