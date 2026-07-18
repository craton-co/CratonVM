# `QuartzAutoConfigurationTests`: `spring.quartz.job-store-type=jdbc` is set but the scheduler still uses `RAMJobStore`

**Status: OPEN — found 2026-07-17**

## Symptom

| Class | tests failed/total |
|---|---:|
| `QuartzAutoConfigurationTests` | 5/24 |

All 5 failures (`withLiquibase`, `withDataSource`,
`dataSourceWithQuartzDataSourceQualifierUsedWhenMultiplePresent`,
`withDataSourceNoTransactionManager`, `withFlyway`) go through the same
shared assertion helper and fail identically:

```
=> java.lang.AssertionError:
Expecting
  org.quartz.simpl.RAMJobStore
to be assignable from:
  [org.springframework.scheduling.quartz.LocalDataSourceJobStore]
but was not assignable from:
  [org.springframework.scheduling.quartz.LocalDataSourceJobStore]
       org.springframework.boot.quartz.autoconfigure.QuartzAutoConfigurationTests.lambda$assertDataSourceInitialized$0(QuartzAutoConfigurationTests.java:398)
       org.springframework.boot.quartz.autoconfigure.QuartzAutoConfigurationTests.withDataSource(QuartzAutoConfigurationTests.java:131)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-quartz.org.springframework.boot.quartz.autoconfigure.QuartzAutoConfigurationTests.out.log`

The asserting code (`QuartzAutoConfigurationTests.java:398`) is:
```java
assertThat(scheduler.getMetaData().getJobStoreClass()).isAssignableFrom(LocalDataSourceJobStore.class);
```
AssertJ's `isAssignableFrom` message prints the **actual** value first —
so the log is saying `scheduler.getMetaData().getJobStoreClass()` really
returned `org.quartz.simpl.RAMJobStore`, not
`LocalDataSourceJobStore`(or any of its ancestors). `RAMJobStore` and
`LocalDataSourceJobStore` are unrelated classes in Quartz's `JobStore`
hierarchy (`LocalDataSourceJobStore` extends `JobStoreCMT` extends
`JobStoreSupport`; `RAMJobStore` is a separate direct `JobStore`
implementation), so this is not a "same name, different `ClassId`"
identity bug — the scheduler genuinely, functionally ended up configured
with the in-memory job store instead of the JDBC-backed one, even though
every one of the 5 failing tests explicitly sets
`.withPropertyValues("spring.quartz.job-store-type=jdbc")` (confirmed by
reading `QuartzAutoConfigurationTests.java:130` in this worktree).

## Root cause (unconfirmed hypothesis)

`QuartzAutoConfiguration.JdbcStoreTypeConfiguration`
(`module/spring-boot-quartz/src/main/java/.../QuartzAutoConfiguration.java`)
is gated by:
```java
@ConditionalOnSingleCandidate(DataSource.class)
@ConditionalOnProperty(name = "spring.quartz.job-store-type", havingValue = "jdbc")
```
Its `dataSourceCustomizer` bean is the only thing that calls
`schedulerFactoryBean.setDataSource(...)`; without it, Quartz's
`SchedulerFactoryBean` falls back to its own default (`RAMJobStore`) —
exactly the observed symptom. Either:
1. CratonVM misevaluates one of these two conditions for this specific
   property/bean-count combination (property binding for
   `spring.quartz.job-store-type` set via
   `AbstractApplicationContextRunner.withPropertyValues` →
   `TestPropertyValues.applyToSystemProperties`, or the
   `@ConditionalOnSingleCandidate(DataSource.class)` bean-count check when
   `DataSourceAutoConfiguration` is also active), so
   `JdbcStoreTypeConfiguration` never activates and its customizer bean is
   never registered; or
2. the customizer bean **is** registered but
   `schedulerFactoryBean.setDataSource(...)`/Quartz's own internal
   `Properties`-driven job-store selection silently no-ops under CratonVM.

Not distinguished between these — no source-level investigation of
`@ConditionalOnProperty`/`@ConditionalOnSingleCandidate` evaluation or of
`SchedulerFactoryBean`'s Quartz `Properties` construction was done this
session. The fastest way to confirm would be a `ConditionEvaluationReport`
dump for this specific test's context (which bean condition, if any,
didn't match) — note this exact reporting mechanism is independently
suspected of not reaching the test process's captured output at all
(see
[`conditionevaluationreport-capturedoutput-empty-cluster.md`](conditionevaluationreport-capturedoutput-empty-cluster.md)),
which may itself frustrate an easy confirmation here.

Note: this is **not** the same bug as
[`../../internal/springboot/disposablebeanadapter-getmethods-hierarchy-duplicate-destroy-method-cluster-FIXED.md`](../../internal/springboot/disposablebeanadapter-getmethods-hierarchy-duplicate-destroy-method-cluster-FIXED.md),
which also affects `module/spring-boot-quartz` (a different class,
`QuartzEndpointWebIntegrationTests`) — that one is a destroy-method
reflection-ambiguity bug during bean *disposal*; this one is a
mis-selected job-store implementation during bean *creation*, with no
exception at all (a clean `AssertionError` on a fully-started context).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-quartz` | `org.springframework.boot.quartz.autoconfigure.QuartzAutoConfigurationTests` |
