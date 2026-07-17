# `HibernateJpaAutoConfigurationTests` HANG: real Hibernate/H2 activity stops after 9 of 83 test methods, then goes silent for the rest of the 300s timeout

**Status: OPEN — found 2026-07-17**

## Symptom

`org.springframework.boot.hibernate.autoconfigure.HibernateJpaAutoConfigurationTests`
(`module/spring-boot-hibernate`) HANGs at the 300s suite timeout
(`results.tsv`: `TIMEOUT`/`HANG`, 300.044s). `.out.log` is completely empty
(0 lines). `.err.log` (784 lines) shows genuine, varied Hibernate ORM/H2
activity — not the generic `InterceptingExecutableInvoker` guard-warning
churn seen in the other HANG clusters this session — for its first ~68s,
then stops producing any further content for the remainder of the run.

The class has **83 `@Test` methods**
(`apps/spring-boot/module/spring-boot-hibernate/src/test/java/.../HibernateJpaAutoConfigurationTests.java`).
Counting `"Building session factory"` occurrences in `.err.log` (Hibernate's
own per-context-boot marker) gives exactly **9** completed session-factory
build/destroy cycles, each showing full, real content (JDBC connection
info, entity/audit metadata processing, statistics init/teardown, etc.):

```
TRACE [org.hibernate.orm.factory] Building session factory
TRACE [org.hibernate.orm.model.mapping.creation] HHH90005701: Wrapping up metadata context...
...
TRACE [org.hibernate.orm.service] HHH010457: Automatically destroying ServiceRegistry after deregistration of every child ServiceRegistry
TRACE [org.hibernate.orm.service] HHH010452: Automatically destroying bootstrap registry after deregistration of every child ServiceRegistry
```

Timestamped lines span **20:25:32.508 to 20:26:40.774** (~68s) for those 9
cycles, then **nothing else appears in the log for the remaining ~232s**
before the suite's watchdog kills the process at 300.044s — a clean, long
silent tail, unlike the "continuous activity right up to the exact kill
point" shape documented in
[`webmvc-error-forward-and-multiboot-timeout-cluster.md`](webmvc-error-forward-and-multiboot-timeout-cluster.md)'s
Cluster B for a similarly many-test-method class. This distinction matters:
if the 9→83 pace had simply continued linearly (9 cycles / 68s ≈ 7.6s/cycle,
so 83 cycles ≈ 630s), the process would still be expected to show
*continuing* (if too-slow) activity throughout the timeout window, not stop
producing any output at all ~130s in.

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-hibernate.org.springframework.boot.hibernate.autoconfigure.HibernateJpaAuto-100579f7d4ad.err.log`

## Root cause

**Not confirmed.** The clean stop (real, varied application-level content
for 68s, then total silence for the remaining ~232s) is a materially
different, more concerning shape than a pure throughput problem — it looks
like the 10th test method (or whichever one runs after the 9th completed
session-factory teardown) gets stuck on something that produces **no**
log output at all: neither Hibernate/H2 activity (would appear if it were
merely a slow 10th DB operation), nor the generic
`InterceptingExecutableInvoker` OOB-guard-warning churn that other genuinely
stuck classes in this rerun show throughout their stall (which would at
least suggest ongoing JUnit-level reflection dispatch). No stack trace,
deadlock indicator, or further diagnostic exists in either log file to
narrow this further. Needs a live thread/stack-dump attach
(`cdb`/`gdb -p <pid>`, this repo's standard technique per
`reference_crash_debug_tooling`) captured mid-hang, plus identifying which
specific test method (by source order or via `-Dtest.method=` if the runner
supports isolating one) is the 10th to run, before a mechanism can be
proposed. Not attempted this session.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-hibernate` | `org.springframework.boot.hibernate.autoconfigure.HibernateJpaAutoConfigurationTests` |
