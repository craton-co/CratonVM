# `batch.BatchTest` — CratonVM ~5.7x slower than HotSpot on MySQL JDBC batching, blows a 120s JUnit timeout

## Status
**OPEN** (2026-08-22, recreated 2026-08-23 after this file vanished from
disk — `C:\craton\CratonVM` is a shared worktree and another concurrent
session was actively running tests in it; not forensically chased down,
just recreated from this session's own record). Confirmed via direct
HotSpot A/B on the identical MySQL setup — a real, new, MySQL-specific
CratonVM performance gap, not a correctness bug, not a test-config artifact.

## Severity
**MEDIUM** — one class, one method, but the gap is large (~5.7x) and specific
to MySQL JDBC batch execution, which is a common real-world path (any app
doing bulk insert/update through Hibernate's batch JDBC support).

## Context

Found while triaging the 50-class FAIL set from the 2026-08-21/22 Hibernate
ORM × MySQL 3-GC full-suite run (see
[`mysql-cross-class-stale-schema-shared-worker-db-20260822.md`](mysql-cross-class-stale-schema-shared-worker-db-20260822.md)
for the full triage). Of that 50, 46 turned out to be a cross-class schema-
reuse artifact and 3 more were pre-existing/not-CratonVM issues (see that
doc's table). This is the one residual that isolates cleanly to a genuine,
new, MySQL-specific CratonVM behavior.

## Evidence

Both runs: isolated single-class invocation, fresh never-used MySQL database,
identical `hibernate.properties`/sysprop MySQL configuration, same Docker
MySQL 8 container.

| Runtime | `@@RESULT` | class wall time |
|---|---|---:|
| HotSpot (Temurin 25.0.3) | `found=4 started=4 ok=4 failed=0` | 24,816 ms |
| CratonVM (ZGC, JIT on) | `found=4 started=4 ok=3 failed=1` | 140,786 ms |

CratonVM's single failure:

```
@@TESTFAIL org.hibernate.orm.test.batch.BatchTest testBatchInsertUpdate(SessionFactoryScope) FAILED
java.util.concurrent.TimeoutException: testBatchInsertUpdate(...) timed out after 120 seconds
```

This is JUnit's own `assertTimeout`/`@Timeout`-style internal budget for the
method (120s), not the harness's external wall-clock cap — CratonVM's own run
of just that one method exceeds 120 seconds, while HotSpot completes the
entire 4-method class (including that method) in under 25 seconds total.

## MySQL-specific, not a general CratonVM-vs-HotSpot throughput gap

This exact class was **not** in the FAIL list from the 2026-08-20 Postgres
3-GC full-suite run — CratonVM handles `BatchTest` fine against Postgres.
Whatever the mechanism, it is specific to the MySQL path (JDBC batch
execution via `com.mysql.cj.jdbc.Driver`, or the `MySQLDialect` batch SQL
Hibernate builds), not a blanket "CratonVM is slower" story.

There is a resolved-but-unrelated prior `BatchTest` doc
([`batchtest-jit-duplicate-batch-insert-unique-violation-20260804.md`](batchtest-jit-duplicate-batch-insert-unique-violation-20260804.md))
— that one was a correctness bug (a `Arrays.equals` JIT defect causing false
unique-constraint collisions), already fixed, and had a completely different
symptom (wrong result, not slowness). Unrelated to this finding beyond
sharing a class name.

## Not yet root-caused

Haven't profiled where CratonVM spends the extra ~115+ seconds. Candidate
areas, not yet checked:
- JDBC batch statement execution path specifically for MySQL (vs. Postgres) —
  `PreparedStatement.addBatch()`/`executeBatch()` round-trips or driver
  interaction overhead
- `com.mysql.cj.jdbc.Driver`-specific code paths CratonVM's native/JDBC glue
  may handle less efficiently than the Postgres driver's
- Whether the slowdown is per-statement (many small batches) or a few large
  ones — `BatchTest.testBatchInsertUpdate`'s actual entity/batch-size shape
  should be checked against Hibernate's `hibernate.jdbc.batch_size` setting

## Next steps

1. Profile (`--stack-sample-ms` or similar) a standalone run of just this
   method to see where the time actually goes — interpreter/JIT compiled
   code vs. native JDBC calls vs. blocked-on-I/O.
2. Try `useServerPrepStmts`/`rewriteBatchedStatements` MySQL driver URL
   parameters (common tuning knobs for JDBC batch throughput) to see whether
   the gap is a driver-configuration difference CratonVM's default connection
   setup doesn't account for, vs. a genuine CratonVM execution-path cost.
3. Check whether other MySQL-batch-heavy classes in the suite show the same
   pattern (this was found via one class; worth a quick sweep of anything
   using `hibernate.jdbc.batch_size` against MySQL specifically).

## Repro

```bash
cd apps/hib-suite-runner
# fresh MySQL db (see the stale-schema doc for container setup)
./cratonvm-zgc-wrapper.sh @common.args -Djava.awt.headless=true \
  -Dhibernate.dialect=org.hibernate.dialect.MySQLDialect \
  -Dhibernate.connection.driver_class=com.mysql.cj.jdbc.Driver \
  -Dhibernate.connection.url="jdbc:mysql://localhost/<fresh-db>?allowPublicKeyRetrieval=true&useSSL=false" \
  -Dhibernate.connection.username=hibernate_orm_test -Dhibernate.connection.password=hibernate_orm_test \
  -Dcraton.batch=1 CratonRunner org.hibernate.orm.test.batch.BatchTest
```

## Related files

- `apps/hibernate-orm/hibernate-core/src/test/java/org/hibernate/orm/test/batch/BatchTest.java`
- [`mysql-cross-class-stale-schema-shared-worker-db-20260822.md`](mysql-cross-class-stale-schema-shared-worker-db-20260822.md) — the full 50-class triage this was extracted from
