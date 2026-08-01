# Liquibase `Scope` per-thread state corruption: scope-id stack mismatch + `AtomicLong` misread as `LogService`

**Status: OPEN — REGRESSED 2026-07-31.**

## Resolution

The original Liquibase `Scope` failures and the later
`ConcurrentReferenceHashMap` casts were one stale-reference failure: the
non-moving young/G1 path could reuse a live reference slot during repeated
application-context refreshes. The moving young collector is now the default,
with JIT root maps controlled by the same shared flag; interpreter-only runs
also default to Generational instead of silently selecting G1. An explicit
`-XX:+UseG1GC` remains an opt-in for callers that deliberately select it.

Validation with `LiquibaseAutoConfigurationTests` passed all 43 tests in both
JIT and `--nojit` modes with zero failures on 2026-07-28.

## Symptom

| Module | Class |
|---|---|
| `module/spring-boot-liquibase` | `org.springframework.boot.liquibase.autoconfigure.LiquibaseAutoConfigurationTests` |

7 of 43 tests in the class fail (`craton-rerun-20260723`,
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard7/logs/module_spring-boot-liquibase.org.springframework.boot.liquibase.autoconfigure.LiquibaseAutoCon-2be69b0874d5.out.log`):
`rollbackFile`, `whenAnalyticsEnabledIsFalseThenSpringLiquibaseHasAnalyticsDisabled`,
`liquibaseConnectionDetailsAreUsedOverLiquibaseProperties`, `overrideDataSource`,
`liquibaseDataSourceIsUsedOverLiquibaseConnectionDetails`,
`lazyConnectionDataSource`, `changelogJson`. Every other test in the class
(including many that also construct a `SpringLiquibase` bean and run real
changesets against an in-memory H2 database) passes — the class is not
uniformly broken, only specific test methods.

Two distinct exception shapes appear, both inside Liquibase's own internal
`Scope` machinery (`liquibase.Scope`, a `ThreadLocal`-backed nested-scope
stack used to carry per-invocation attributes like the active `Database`,
`LogService`, and a UI/analytics context):

**Shape 1 — scope-id stack mismatch** (`rollbackFile`):

```
=> java.lang.IllegalStateException: Unstarted application context ... failed to start
 Caused by: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'liquibase' ...: liquibase.exception.LiquibaseException: java.lang.RuntimeException: Cannot end scope fszstiqzek when currently at scope fmmcaghten
 Caused by: liquibase.exception.LiquibaseException: liquibase.exception.LiquibaseException: java.lang.RuntimeException: Cannot end scope fszstiqzek when currently at scope fmmcaghten
   liquibase.integration.spring.SpringLiquibase.afterPropertiesSet(SpringLiquibase.java:289)
 Caused by: liquibase.exception.LiquibaseException: java.lang.RuntimeException: Cannot end scope fszstiqzek when currently at scope fmmcaghten
   liquibase.Liquibase.runInScope(Liquibase.java:1368)
   liquibase.Liquibase.futureRollbackSQL(Liquibase.java:986)
   liquibase.integration.spring.SpringLiquibase.generateRollbackFile(SpringLiquibase.java:304)
   liquibase.integration.spring.SpringLiquibase.lambda$afterPropertiesSet$0(SpringLiquibase.java:278)
   liquibase.Scope.lambda$child$0(Scope.java:241)
   liquibase.Scope.child(Scope.java:240)
   liquibase.integration.spring.SpringLiquibase.afterPropertiesSet(SpringLiquibase.java:272)
 Caused by: java.lang.RuntimeException: Cannot end scope fszstiqzek when currently at scope fmmcaghten
   liquibase.Scope.exit(Scope.java:288)
   liquibase.Scope.child(Scope.java:226)
   liquibase.command.CommandScope.execute(CommandScope.java:257)
   liquibase.Scope.lambda$child$0(Scope.java:241)
   liquibase.Scope.child(Scope.java:240)
   liquibase.Liquibase.runInScope(Liquibase.java:1366)
```

**Shape 2 — `AtomicLong` misread where a `LogService` was expected** (the
other 6 failures, e.g. `whenAnalyticsEnabledIsFalseThenSpringLiquibaseHasAnalyticsDisabled`):

```
=> java.lang.IllegalStateException: Unstarted application context ... failed to start
 Caused by: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'liquibase' ...: Failed to instantiate [liquibase.integration.spring.SpringLiquibase]: Factory method 'liquibase' threw exception with message: java.util.concurrent.atomic.AtomicLong cannot be cast to liquibase.logging.LogService
 Caused by: org.springframework.beans.BeanInstantiationException: Failed to instantiate [liquibase.integration.spring.SpringLiquibase]: ...
 Caused by: java.lang.ClassCastException: java.util.concurrent.atomic.AtomicLong cannot be cast to liquibase.logging.LogService
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard7/logs/module_spring-boot-liquibase.org.springframework.boot.liquibase.autoconfigure.LiquibaseAutoCon-2be69b0874d5.out.log`
(lines 833-1681 for the 7 failure blocks; the surrounding ~800 lines are
passing tests' Liquibase changelog INFO output, useful context showing
`Scope`-driven changelog runs succeeding normally dozens of times in the same
process before/around the failures).

## Original diagnosis (superseded)

`liquibase.Scope` (third-party jar, not in this worktree's source — not
decompiled this session) is a `ThreadLocal<Scope>`-backed nested-scope stack:
each `Scope.child(...)` call pushes a new scope (with a random string `id`)
as the new "current" scope for the thread and runs a block inside it;
`Scope.exit(id)` pops it back off, asserting the `id` being exited matches
the scope currently on top of the stack (hence "Cannot end scope X when
currently at scope Y" when it doesn't). Separately, `Scope` also holds a
per-scope attribute map (keyed by string, e.g. `"logService"`) that
`SpringLiquibase`'s bean-creation path reads back with a checked cast to
`LogService` — the log strongly suggests the same map (visible in the
surrounding log output, e.g. `"Using deploymentId: 4824541706"`, a
plain/atomic numeric ID Liquibase stores per-scope) is what's returning an
`AtomicLong` where a `LogService` was expected.

Both shapes point at the **same underlying mechanism**: `Scope`'s per-thread
nested state (the id stack and/or the attribute map) getting corrupted or
cross-contaminated across the many sequential `Scope.child()` invocations
this one JUnit-forked process runs (43 test methods in this class, most of
which independently create+run+dispose a full `SpringLiquibase`/H2 cycle in
the same thread) — i.e. **not** a single-call bug, but a state-leak or
wrong-slot-write across successive invocations, which is why most calls
succeed and only some fail (order/timing-dependent). This is the same shape
of bug family as the already-documented `OnClassCondition.addAll` NPE-cast
cluster and the HashMap-native-dispatch-cache corruptions referenced
elsewhere in this docs tree (a lookup by key returning a value belonging to
an unrelated key) — but this session did not confirm whether the underlying
storage is a native `HashMap`/`ThreadLocal` implementation detail on
CratonVM's side, or a genuine Liquibase-internal reentrancy bug that happens
to be newly exposed by different scheduling/timing on CratonVM. **Not
confirmed by attaching a debugger or reading Liquibase's actual `Scope`
bytecode this session** (out of scope for a log-reading-only triage pass) —
flagged as the strongest next step: decompile `liquibase-core`'s `Scope`
class, find its `ThreadLocal` field and attribute-map implementation, and
check whether CratonVM's `ThreadLocal` get/set or the attribute map's
backing `HashMap`/`Map` implementation could serve a stale or
wrong-slot value under the pattern this class's `@Test` methods use (each
method independently calling `contextRunner.run(...)`, which restarts
Liquibase's scope machinery fresh in the same thread every time).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-liquibase` | `org.springframework.boot.liquibase.autoconfigure.LiquibaseAutoConfigurationTests` (7/43 methods: `rollbackFile`, `whenAnalyticsEnabledIsFalseThenSpringLiquibaseHasAnalyticsDisabled`, `liquibaseConnectionDetailsAreUsedOverLiquibaseProperties`, `overrideDataSource`, `liquibaseDataSourceIsUsedOverLiquibaseConnectionDetails`, `lazyConnectionDataSource`, `changelogJson`) |
| `module/spring-boot-hibernate` | `org.springframework.boot.hibernate.autoconfigure.HibernateJpaAutoConfigurationTests` (1/70: `testLiquibasePlusValidation`, 2026-07-31 regression) |

## Regression note (2026-07-31)

Recurred in a full-suite rerun (`craton-rerun-20260731`, `all-jit`) as a
single failure in a different class,
`org.springframework.boot.hibernate.autoconfigure.HibernateJpaAutoConfigurationTests#testLiquibasePlusValidation`
(`module/spring-boot-hibernate`). This is exactly Shape 1 from the original
report (scope-id stack mismatch), unchanged down to the wording:

```
org.springframework.beans.factory.BeanCreationException: Error creating bean
with name 'entityManagerFactory' ...: Failed to initialize dependency
'liquibase' of LoadTimeWeaverAware bean 'entityManagerFactory': Error
creating bean with name 'liquibase' defined in
org.springframework.boot.liquibase.autoconfigure.LiquibaseAutoConfiguration$LiquibaseConfiguration:
java.lang.RuntimeException: Cannot end scope qmmhkkrlxm when currently at
scope kphqthcfbd
  liquibase.Scope.exit(Scope.java:288)
  liquibase.Scope.child(Scope.java:252)
  liquibase.Scope.child(Scope.java:240)
  liquibase.Scope.child(Scope.java:219)
  liquibase.integration.spring.SpringLiquibase.afterPropertiesSet(SpringLiquibase.java:272)
```

Log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260731/all-jit/logs/module_spring-boot-hibernate.org.springframework.boot.hibernate.autoconfigure.HibernateJpa-100579f7d4ad.out.log`
(the class's overall run took 1084959 ms / ~1085s, close to but under the
suite's 1200s effective timeout for this class — this is a genuine single-test
assertion failure at the end of a long run, not a timeout artifact: the JUnit
Platform summary cleanly reports `70 tests started, 69 successful, 1 failed`
and prints the full stack trace above).

The 2026-07-28 "Resolution" above attributed both this shape and the
`AtomicLong`/`LogService` shape to a stale-reference bug in the pre-moving-young
GC path, closed by making the moving young collector the default. This rerun
was on a build where moving young is still the default (see the `[jit]
optimizing (C2/IR) tier DISABLED: the moving young generation is active`
banner in the paired `.err.log`), so either that fix was incomplete/narrower
than believed, or a separate regression re-broke the same Liquibase `Scope`
code path. Substantial JIT/moving-young churn landed between 2026-07-28 and
2026-07-31 (e.g. `ea5b2df6e` "keep raw JIT-to-JIT direct calls gated on the
moving-young flag", `11901e9a6` "veto moving-young on frame LIVENESS, not on
compiled code existing", `ea874f3c6` "refuse to compile an invokedynamic trap
that cannot be resumed", `f78b72670` "scope the relocation-safety gates so
the optimizing tier runs again") — any of these touch exactly the
relocation/root-mapping machinery this bug's original two "Resolution"s (the
2026-07-07 `jit_scan`/invokedynamic fix and the 2026-07-28 moving-young
default) both hinged on. Not re-diagnosed to a specific commit this session —
flagged as the next place to bisect.
