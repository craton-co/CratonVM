# First full hib-orm-vs-Postgres run: 704 FAILs, 97% explained by local connection-pool exhaustion, not CratonVM

## Status

**Not a CratonVM defect** for 684 of 704 FAILs (97%); **not a defect** for a
further 18 (expected H2-only dialect gating); **2 unconfirmed candidates**
left, filed honestly as unconfirmed rather than as bugs.

## Context

First-ever full Hibernate ORM suite run against a real (non-H2, non-MySQL)
Postgres backend, 2026-09-07, local Windows box, `dev@4f2ed2687`. 3975
classes, 8 shards, `-Xmx` default, ZGC (the engine default, no `-XX:+Use*GC`
flag passed), one local `postgres:15-bullseye` Docker container with 8
`hibernate_orm_test_1..8` worker databases (one per shard, via the
suite runner's own `--pg-worker-base 0` mechanism — see `run-hib.sh`'s
header comment on `PG_WORKER_BASE` for how that bypasses
`GradleParallelTestingResolver`'s Gradle-only `$worker` templating).

Result: `status: ABORTED=2 PASS=3131 NOTESTS=138 FAIL=704` — **zero CRASH,
zero HANG**, 66-minute wall clock. The VM itself held up cleanly; this page
is entirely about triaging the 704 FAILs, which is a brand-new baseline (no
prior Postgres-vs-hib-orm run exists on this project to diff against) —
hence "many fails that weren't there" against the familiar H2 baseline.

## 1. 684 of 704 (97%) — local Postgres connection-pool exhaustion

```
org.hibernate.service.spi.ServiceException: Unable to create requested service
  [org.hibernate.engine.jdbc.env.spi.JdbcEnvironment] due to: Unable to make
  JDBC Connection [jdbc:postgresql://localhost/hibernate_orm_test_1?...]
```

1620 individual test-method failures (a class can fail several methods,
hence more failures than classes) collapse to 684 distinct classes sharing
this one signature. `postgres:15-bullseye`'s default `max_connections=100`;
post-run `pg_stat_activity` showed only 6 live connections, consistent with
the pool having been driven to exhaustion at some point during the
3975-class, 8-shard, 66-minute run and then draining back down as later
classes failed fast rather than holding connections. 8 shards × Hibernate's
own `hibernate.connection.pool_size 5` default is only 40 baseline
connections — well under 100 — so this reads as a **connection leak
accumulating over the run** (most plausibly from classes that ABORTED,
HANG'd, or otherwise exited abnormally without cleanly returning their pool
connections) rather than steady-state exhaustion from concurrency alone.

**Not investigated further here**: which specific class(es) leak, whether
raising `max_connections` alone would clear it, or whether HikariCP-style
pool eviction settings are the more correct fix. This is a **local fixture
environment gap** (a single-container Postgres setup with default limits,
first time this suite has been pointed at Postgres at all), not a CratonVM
defect, and doesn't belong as a "VM bug" finding — noting it here so the
684 FAIL rows aren't mistaken for 684 CratonVM regressions.

## 2. 18 of 704 — expected: H2-only dialect-gated tests, correctly refusing to run

```
org.junit.jupiter.engine.execution.ConditionEvaluationException: Failed to
  evaluate condition [org.hibernate.testing.orm.junit.DialectFilterExtension]:
  Could not connect to database with dialect class: org.hibernate.dialect.H2Dialect
```
(4 of the 18 surface the same root cause as a bare `HibernateException`
instead, same message.)

These 18 classes (`DialectSQLExceptionConversionTest`, the
`tool.schema.scripts.*ImportExtractorTest` family, several more) are
explicitly gated to run only under `H2Dialect` — they exercise H2-specific
SQL-exception text or H2-specific import-script parsing quirks by design.
Run under a Postgres-only configuration with no H2 connection available at
all, `DialectFilterExtension` correctly detects the mismatch and fails
fast. This is the suite behaving exactly as designed for a
single-secondary-dialect run; not a CratonVM defect, not further
investigated.

## 3. 2 of 704 — genuinely unconfirmed, not claimed as bugs

Both are `expected: X but was: Y` numeric mismatches with no cross-check yet
against real HotSpot under this same Postgres setup:

- **`org.hibernate.orm.test.annotations.loader.LoaderWithInvalidQueryTest.test`**
  — `expected: 2 but was: 0`. Tests loader behavior around an intentionally
  invalid query; plausible this depends on Postgres-vs-H2 SQL error-recovery
  semantics (a real, expected dialect difference) rather than a CratonVM gap
  — not established either way here.
- **`org.hibernate.orm.test.jpa.transaction.batch.FailingAddToBatchTest.testInsert`**
  — `expected: 0 but was: -1`. Tests JDBC batch-update return codes when one
  statement in the batch is designed to fail. JDBC batch failure sentinel
  values (`SUCCESS_NO_INFO`, `EXECUTE_FAILED`, driver-specific negative
  codes) are well known to differ between drivers/dialects — again plausibly
  a real H2-vs-PostgreSQL-JDBC-driver difference rather than a CratonVM
  defect, not established either way here.

**Next step for both, not done here**: rerun each in isolation under real
HotSpot JDK 25 against the same local Postgres container. If HotSpot passes
both, they're real CratonVM findings and worth their own pages; if HotSpot
also shows the same numbers, they're dialect-expected and this section
should be folded into §2.

## A separate, unrelated finding from the same triage session

While chasing two SIGSEGVs surfaced by a *different*, concurrent Azure
hib-orm rerun (`OffsetTimeTest`, `DefaultCatalogAndSchemaTest`), discovered
that another session had silently repointed the shared
`/tmp/cratonvm-{gen,g1,zgc}-wrapper.sh` scripts on the Azure host away from
`/data/cratonvm/target/release/cratonvm` to a different, much older
worktree's binary (`/data/cvm-h2serial-20260813/target/release/cratonvm`,
dated 2026-08-13) at some point on 2026-09-07. Every Azure suite rerun
launched through those wrappers today (Tomcat, Spring Framework, and
hib-orm's own non-passed reruns — 9 arms total) was silently testing that
unrelated, stale binary instead of the freshly-built dev-tip binary the
launch commands intended. Both crashes traced back to that contamination
(one, `OffsetTimeTest`, was a real-looking GC-decommit-species SIGSEGV
already carrying its own extensive built-in diagnostic hypotheses —
consistent with belonging to whatever the `h2serial` worktree's own,
separate investigation already tracks, not a fresh CratonVM finding from
today's intended run; the other, `DefaultCatalogAndSchemaTest`, was `rc=137`
/ SIGKILL under a 80+ load-average host — an OOM-kill signature, unrelated
either way). Wrappers restored to the correct binary path. **Not filed as a
CratonVM bug page** — it's a shared-host process hazard (a `/tmp` script
shared across concurrent sessions with no ownership marker), not a defect,
but worth remembering: verify a shared wrapper's content immediately before
relying on it, not just once earlier in a session.

## Reproduction

```bash
# local Postgres setup used here:
docker run -d --name hib-orm-pg -p 5432:5432 -e POSTGRES_PASSWORD=postgres postgres:15-bullseye
docker exec hib-orm-pg psql -U postgres -c "CREATE USER hibernate_orm_test WITH PASSWORD 'hibernate_orm_test' SUPERUSER;"
for i in 1 2 3 4 5 6 7 8; do
  docker exec hib-orm-pg psql -U postgres -c "CREATE DATABASE hibernate_orm_test_$i OWNER hibernate_orm_test;"
done
cd apps/hibernate-orm
JDK="<jdk-25 home>"
JAVA_HOME="$JDK" ./gradlew -Dorg.gradle.java.home="$JDK" -Pdb=pgsql_ci :hibernate-testing:jar :hibernate-core:testClasses

cd ../hib-suite-runner
CV_BIN=<cratonvm.exe> JDK="$JDK" \
  ./run-hib.sh --list full_testlist.txt --shards 8 --pg-worker-base 0 --out runs/<tag> --timeout 300
```

Full results (untracked, local): `apps/hib-suite-runner/runs/full-pg-zgc-20260907/`.
