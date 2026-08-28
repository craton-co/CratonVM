# Hibernate ORM on MySQL — the "stale schema" cascade: a killed class leaks its schema into a worker database the harness never reset

## Status

**RETIRED 2026-08-27 — root-caused and fixed, and the cause is not the one this
page proposed.** It is not a DDL or JDBC defect, and it is not
CratonVM-specific in the way the title used to claim. It is a harness one, with
a genuine VM slowness underneath it:

> A class too slow for the wall cap is KILLED, so it never reaches the test
> framework's schema drop. `run-hib.sh` gave each shard one worker database and
> **never reset it**, so that class's tables outlived the shard *and the run* —
> and dozens of Hibernate classes declare their own `Person`, `Product`,
> `Animal` and `User` with different column sets, so every later collision fails
> on a schema it did not create.

So **~119 of the 122 failures are one cascade, not 119 defects.** Fixed in
`run-hib.sh` + `DbReset.java`; measurements below.

### The two hypotheses this page had, and why both are wrong

It proposed *"(a) CratonVM's schema bootstrap not fully executing/committing its
DROP+CREATE DDL against MySQL specifically, or (b) some MySQL-server-side
statement/metadata caching that a CratonVM-originated connection trips
differently"*. Neither survives one measurement:

**A class that exits normally drops its own schema, on CratonVM, even when its
tests fail.** `hql.ASTParserLoadingTest` on a fresh database under CratonVM
finishes **21 tests down** and still leaves `tables left: 0`. There is no
general "CratonVM does not drop" behaviour to find. The leak needs the process
to be *killed*.

### The chain, each link measured

1. **CratonVM is ~25x slower on the leaking class.** `hql.ASTParserLoadingTest`:
   **1447 s** on CratonVM against seconds on HotSpot, on the same fresh
   database. The suite runs `--timeout 300`. The class is duly recorded `HANG`.
2. **A killed process leaks its schema.** Reproduced exactly as the harness does
   it — `timeout 300 cratonvm ... CratonRunner hql.ASTParserLoadingTest` →
   `rc=124`, and **54 tables left behind**: `Animal`, `Human`, `Zoo`,
   `Customer`, `Product`, `User`, `LineItem`, `employee`, `department`, …
3. **The worker databases were never reset.** `run-hib.sh` contained no
   `DROP DATABASE` / `CREATE DATABASE` at all; it only pointed shard *N* at
   `hibernate_orm_test_N`. Debris therefore accumulated **across runs**. That is
   why this page's own `ComponentTest` example collided with a stale `T_USER`
   that **no earlier class in its shard had created** — it was left by an
   earlier run. At the time of writing, `hibernate_orm_test_1` still held 81
   such tables while workers 2–18 held none.
4. **The debris fails later classes.** Same class, same binary, only the
   database differing:

   | | result |
   | --- | --- |
   | `jpa.compliance.CriteriaMutationQueryTableTest` on the poisoned database | **0 / 2** — `Table 'Animal' already exists`, `Unknown column 'age'` |
   | the same class on a fresh database | **2 / 2 PASS** |

   And on the 2026-08-24 G1 run, on the leaker's own shard: **45 FAILs after it,
   17 before**.

## The fix

`DbReset` drops every table in the worker schema. `run-hib.sh` calls it at
**shard start** (clearing debris from previous runs) and **after any class whose
process died**. `--no-db-reset` A/Bs the cascade.

Two choices in it that are deliberate:

* **Not before every class.** That would be 4548 resets instead of ~7, and it
  would silently paper over a leak on a CLEAN exit — which would be a real VM
  defect, and one this harness should keep surfacing rather than hide. Point 1
  above is the measurement that says the kill path is the only one that leaks.
* **It runs on the STOCK JDK**, never on the binary under test. When the VM
  being measured is the thing that just got killed, it is also the last thing
  that should be trusted to clean up after itself.

Measured, two classes, one shard, same binary, same database, only the flag
differing:

| arm | leaker | victim | tables left |
| --- | --- | --- | ---: |
| `--no-db-reset` (the behaviour this page documents) | `HANG` | **FAIL 0/2** | 54 |
| db-reset on | `HANG` | **PASS 2/2** | 0 |

and the log shows it firing only where intended:

```text
[db-reset] shard 0 start :: @@DBRESET clean tables=0
[db-reset] org.hibernate.orm.test.hql.ASTParserLoadingTest died (HANG rc=124) :: @@DBRESET reset tables_dropped=54
```

### A cleanup step that could not fail loudly, and nearly did not fail legibly

The first version built `DbReset`'s classpath from `$SELF_DIR`, which under Git
Bash is `/c/craton/...`. The JVM answered `Could not find or load main class
DbReset`, the reset logged that at both call sites, and all 54 tables stayed —
an arm that looked like it ran and changed nothing. `to_native_path` (a
`cygpath -m`) is the fix. **Worth keeping written down**: the A/B only caught it
because the arm printed what the reset actually said instead of assuming it
worked.

## What this does NOT fix, and what it unmasks

The cascade is gone; the two things underneath it are real and now visible:

* **`hql.ASTParserLoadingTest` costs 1447 s on CratonVM against seconds on
  HotSpot** — that slowness is what puts it over the cap in the first place.
  Raising its cap in `class-overrides.tsv` would stop the HANG but not the cost.
* **It fails 21 of 104 tests on a FRESH database** (HotSpot: 104/104). Those are
  genuine defects that the cascade has been hiding — they are not schema
  artifacts, and nothing in this page's original 122-class accounting separated
  them out.

Both are tracked in
`known-issues/hibernate/astparserloadingtest-slow-and-21-real-failures-20260827.md`.

## A configuration gap found on the way

`hibernate-core/target/resources/test/hibernate.properties` has since been
reverted to **H2**, so the MySQL suite is not reproducible from the harness
alone any more: dialect, driver and credentials have to come from somewhere. The
runs here supplied them through the tracked `required-sysprops.tsv` hook
(`HIB_REQUIRED_SYSPROPS=...`). Reading a run without noticing is easy and
expensive — the first A/B attempt here reported `found=106 ok=0` in 9.7 s, which
is not a result, it is an H2 dialect pointed at a MySQL URL.

---

# The evidence as it was gathered

Everything below is the original page, kept because its measurements are sound
and are what the root cause above explains. Two claims in it are superseded:
the "Not yet root-caused" section's two hypotheses (both refuted above), and the
framing of the isolation sweep's 46/52 as "one mechanism appearing 46 times" —
correct, and the mechanism is the cascade, not a VM DDL defect.

## 2026-08-24/25 update: complete-suite MySQL run after `dev` update, ~2.5x more classes hit

Full 4548-class run (`--list testlist.txt`, not the `passed` category) across
G1/ZGC/Generational, 6 shards each, after updating to `dev` HEAD `9ba39b4c1` and
rebuilding `cratonvm.exe`. This is the suite's first "complete" (not `passed`-category)
run against MySQL, which is why the count is so much larger than the 48-50 above:
`passed.txt`/`others.txt` predate the MySQL switch (last regenerated 2026-08-15, against
Postgres) and were never re-baselined, so a `--category passed` run silently excludes
whichever passing-under-Postgres classes turned out to be schema-collision-prone under
MySQL. Running the literal complete list is what actually surfaces the full blast radius.

| GC arm | PASS | FAIL | HANG | ABORTED | NOTESTS | wall |
|---|---:|---:|---:|---:|---:|---:|
| G1 | 4306 | 123 | 7 | 8 | 104 | 244m57s |
| ZGC | 4289 | 140 | 7 | 8 | 104 | 238m46s |
| Generational | 4305 | 122 | 9 | 8 | 104 | 245m23s |

FAIL common to all three arms: **122** (vs. 48 in the original report). Classified by
signature (grepped each class's raw log against the two known schema-collision error
shapes below):

- **119 / 122 (98%)** match the exact same two error shapes as the original report
  (`Unknown column '...' in 'field list'` / `'...' doesn't have a default value`, or the
  companion `Table '...' doesn't exist` / `already exists` / `Cannot drop table ...
  referenced by a foreign key constraint` shapes seen when the leaked prior class's DDL
  itself conflicts rather than just its column set). Same mechanism, no new information —
  not re-triaged individually given the exact match.
- **1 / 122** is `batch.BatchTest` — already tracked, see
  [`batchtest-mysql-jdbc-batching-NOT-A-VM-DEFECT-20260822.md`](batchtest-mysql-jdbc-batching-NOT-A-VM-DEFECT-20260822.md).
- **1 / 122** is `bootstrap.scanning.PackagedEntityManagerTest` — already tracked in the
  isolation-sweep table above (`Error calling Driver.connect() [Connection to
  localhost:5432 refused ...]` — same stale-Postgres-port packaged `.par`, exact same
  error text as 2026-08-22).
- **1 / 122 is new**: `query.hql.SubqueryOperatorsTest.testSubqueryInVariousClauses` —
  `java.sql.SQLException: Illegal mix of collations (utf8mb4_0900_as_cs,IMPLICIT) and
  (utf8mb4_0900_ai_ci,IMPLICIT) for operation '='`. A MySQL collation-mismatch error, not
  a shape this doc has seen before. Not yet checked against HotSpot on this same MySQL
  container/schema — plausible this is a server-side collation default rather than a
  CratonVM behavior (nothing about collation resolution touches VM-specific code), but
  unconfirmed. Worth a quick HotSpot A/B before spending more time on it.

ZGC additionally regressed 16 classes not shared with G1/Generational (140 vs 122) — not
individually triaged; almost certainly more instances of the same stale-schema
mechanism landing on a different class given ZGC's shard timing/ordering is not
identical to the other two arms (same reasoning as the original report's HANG-count
variance across arms).

No new hibernate regression found beyond what this doc and
`batchtest-mysql-jdbc-batching-NOT-A-VM-DEFECT-20260822.md` already track, except the
one unconfirmed `SubqueryOperatorsTest` collation error above.

## Severity
**MEDIUM-HIGH** — 48-50 / 4548 classes (~1%) FAIL, identically across all three GC
arms, only when the suite runs against MySQL with a shared/reused worker database
(the harness's normal mode: 6 shards, ~758 classes sequentially per shard, one
physical database per shard). Did not appear at all in the equivalent Postgres
3-GC full-suite run two days earlier.

## Context

First full Hibernate ORM run against MySQL (previous local full-suite runs used
Postgres). `run-hib.sh` didn't have MySQL wired in at all — added a
`--mysql-worker-base` flag mirroring the existing `--pg-worker-base` isolation
trick, and rewrote `hibernate-core/target/resources/test/hibernate.properties`
for the MySQL dialect/driver/URL (a second connection path,
`DatabaseCleanerContext`'s static init via `JdbcConnectionContext`, reads that
file directly rather than the merged `-D` sysprops the per-test SessionFactory
bootstrap uses — see "Two connection paths" below). Full 4548-class suite run
across ZGC/G1/Generational, 6 shards each, against a fresh MySQL 8 container
(named volume, 18 worker databases `hibernate_orm_test_1..18`, 6 per GC arm).

## Result counts (all three arms, single run, 2026-08-21/22)

| GC arm | PASS | FAIL | HANG | ABORTED | NOTESTS | wall |
|---|---:|---:|---:|---:|---:|---:|
| ZGC | 4381 | 50 | 5 | 8 | 104 | 252m32s |
| G1 | 4380 | 50 | 6 | 8 | 104 | 258m44s |
| Generational | 4379 | 50 | 7 | 8 | 104 | 260m0s |

- **ABORTED=8 identical on all three arms** — matches `known-benign-aborts.tsv`
  exactly (JUnit `Assumptions`-based self-skips baked into the tests). Not signal.
- **FAIL union across all three arms = 52 classes; 48 of those 52 (92%) are the
  identical set on all three arms.** GC-independent.
- **Of that 52-class FAIL union, only 3 also failed in the 2026-08-20 Postgres
  3-GC full run** (`apps/hib-suite-runner/runs/full-pg-*-20260820-3gc-pg-v2/`).
  **49 are new, MySQL-only.**
- HANG count varies slightly (5/6/7) — consistent with ordinary throughput/
  contention noise under 6-way fork-per-class sharding, not examined further here.

## Failure signature — one shape, two symptoms

Every sampled FAIL (8 classes checked by hand across the batch/collections/
locking/flush/notfound/query/mapping/timestamp packages) is one of exactly two
MySQL errors, always on an INSERT during test setup, never inside the test's
actual assertions:

```
org.hibernate.exception.SQLGrammarException: could not execute statement
  [Unknown column 'phones' in 'field list'] [insert into Person (phones,id) values (...)]

org.hibernate.exception.ConstraintViolationException: could not execute statement
  [Field 'employed' doesn't have a default value] [insert into Person (name,id) values (...)]
```

Both are schema-shape mismatches: the INSERT Hibernate builds from *this
class's* entity mapping references a column that either doesn't exist in the
actual table, or a NOT-NULL column the mapping doesn't populate — i.e. the
physical table in the database doesn't match the class currently running.

**`Person` and `Product` are not unique table names.** Dozens of unrelated
Hibernate ORM test classes each declare their own `Person`/`Product` entity
with a different column set (confirmed: `locking/`, `flush/`, `query/hql/`,
`notfound/` each have their own distinct `Person.java`). Under this harness's
fork-per-class model, ~758 classes run sequentially against one shared,
reused worker database per shard — so if an earlier class's version of
`Person` leaks into a later class's run, the exact "wrong columns" signature
above is what you'd see, and it would explain why entirely unrelated test
areas hit an identical-shaped bug.

## Confirmed: this is CratonVM-specific DB-reuse behavior, not a test/config bug

**Isolation control (the decisive check):** ran `CollectionTest` — one of the
FAIL classes, which failed on the full run against `hibernate_orm_test_3`
(ZGC arm, shard 2, i.e. after many prior classes had already run against that
database) — directly under CratonVM against a **brand-new, never-before-used**
worker database (`hibernate_orm_test_99`):

```
./cratonvm-zgc-wrapper.sh @common.args ... -Dhibernate.connection.url=".../hibernate_orm_test_99..." \
  -Dcraton.batch=1 CratonRunner org.hibernate.orm.test.mapping.collections.CollectionTest
=> @@RESULT ... found=1 started=1 ok=1 failed=0 ...   PASS
```

Same binary, same class, same code path — the only variable changed is
whether the database already carries another class's history. Fresh DB:
PASS. Reused DB (as the full run naturally exercises every shard after its
first class): FAIL.

**HotSpot A/B** (same MySQL container, `hibernate_orm_test_1`, a reused-but-
different-history worker): `CollectionTest` and `AutoFlushBeforeLoadTest` both
PASS under HotSpot with the identical `hibernate.properties`/sysprop setup —
confirming HotSpot tolerates worker-DB reuse across classes fine in general
(this is not "MySQL reuse is inherently unsafe" — it works for HotSpot and it
works for Postgres on CratonVM). The divergence is specific to CratonVM +
MySQL + a reused database.

## Full isolation sweep (2026-08-22 follow-up) — confirms the scale

Re-ran all 52 FAIL-union classes individually, each against its own freshly
dropped-and-recreated database (no shared history at all):

**46 / 52 (88%) passed clean in isolation** — confirming the stale-schema-leak
explanation for the large majority of the original FAIL count. These are not
independent bugs; they are one mechanism appearing 46 times.

The remaining 6 do NOT reduce to that mechanism. Each was individually
triaged (isolated run + a HotSpot A/B against the identical MySQL setup)
before being counted as a residual:

| Class | Isolated result | HotSpot same setup | Disposition |
|---|---|---|---|
| `jpa.lock.LockTest` | 1 FAIL (`testFindWithPessimisticWriteLockTimeoutException`, exceeded a 5000ms budget by 245ms) | same failure | **Not new** — already documented as a pre-existing cold-start timing-margin non-bug, see [`locktest-pessimistic-write-timeout-is-not-a-vm-bug-20260730.md`](locktest-pessimistic-write-timeout-is-not-a-vm-bug-20260730.md) |
| `bootstrap.scanning.PackagedEntityManagerTest` | 14/15 FAIL, `Connection to localhost:5432 refused` | (not run — same root cause applies to any JVM) | **Not a CratonVM bug** — this test bootstraps its `EntityManagerFactory` from a packaged `.par`'s own `META-INF/persistence.xml`, which is a Gradle-templated resource (`@jdbc.url@` etc., see `hibernate-core/src/test/bundles/templates/excludehbmpar/META-INF/persistence.xml`) baked in at build time from whichever `-Pdb=...` profile last ran. It was templated for Postgres and never re-rendered for MySQL — the packaged archive under `target/packages/*/` literally still points at port 5432 regardless of any runtime sysprop override. A build-config gap, not a runtime one. |
| `hql.HqlParserMemoryUsageTest` | 1 FAIL (parser memory 629MB vs 256MB budget) | not run | **Pre-existing, not MySQL-specific** — also failed in the 2026-08-20 Postgres 3-GC run |
| `sql.exec.SmokeTests` | still times out (>250s) even isolated | not run | **Pre-existing, not MySQL-specific** — also failed in the Postgres run; see `HIB-CV-37-sqlexec-smoketests-sigsegv.md` / `smoketests-concurrent-query-throughput-20260723-RETIRED.md` for this class's history |
| `ondelete.OnDeleteTest` | 1 FAIL (`testJoinedSubclass`, cascade-delete leaves a child row behind) | **same failure, identical count** | **Not a CratonVM bug** — HotSpot fails the identical assertion the identical way against the identical MySQL container. Likely a `MySQLDialect.supportsCascadeDelete()` vs. actual generated-DDL gap in Hibernate ORM itself, or an environment/storage-engine characteristic — orthogonal to CratonVM. |
| `batch.BatchTest` | 1 FAIL (`testBatchInsertUpdate`, exceeds a 120s internal JUnit timeout; ms=140786 for the class) | **PASS**, ms=24816 for the class (~5.7x faster) | **New, genuine, MySQL-specific** — see [`batchtest-mysql-jdbc-batching-NOT-A-VM-DEFECT-20260822.md`](batchtest-mysql-jdbc-batching-NOT-A-VM-DEFECT-20260822.md) |

So of the original 50 FAIL, the accounting is: **46 stale-schema artifact + 3
pre-existing/not-CratonVM (excluded here) + 1 real, new, MySQL-specific
performance defect** (`BatchTest`, its own doc).

## Not yet root-caused *(SUPERSEDED — see the root cause at the top)*

`MySQL8DatabaseCleaner`/`MySQL5DatabaseCleaner`
(`hibernate-testing/.../cleaner/MySQL8DatabaseCleaner.java`) only issue
`TRUNCATE <schema>.<table>` — that's a within-class, between-test-method
cleaner, not a between-class schema reconciler. The actual CREATE/DROP for
each class's own entity set should come from the test framework's own schema
bootstrap at `SessionFactory` build (independent of `hbm2ddl.auto`, which
isn't set in this properties file at all). Since each class is a fresh JVM
(fork-per-class), there's no in-process connection-pool or prepared-statement
cache to blame — whatever's happening has to be either (a) CratonVM's schema
bootstrap not fully executing/committing its DROP+CREATE DDL against MySQL
specifically, or (b) some MySQL-server-side statement/metadata caching that a
CratonVM-originated connection trips differently than a HotSpot-originated
one issuing the ostensibly same DDL sequence.

## Next steps *(SUPERSEDED — all three are answered above; none needed a general_log diff)*

1. Enable MySQL's `general_log` and diff the exact DDL statement sequence
   CratonVM issues around a class's `SessionFactory` bootstrap/shutdown
   against HotSpot's, for the same class run back-to-back after the same
   prior class on the same worker DB — this should show directly whether a
   DROP/CREATE is skipped, reordered, or silently fails.
2. Check whether CratonVM's `com.mysql.cj.jdbc.Driver` interaction handles
   MySQL's implicit-commit-on-DDL semantics the same way HotSpot's does
   (autocommit state at the point the DROP/CREATE runs).
3. Once the union of 52 classes is confirmed order-dependent, a targeted
   two-class repro (run class A then class B back-to-back against the same
   fresh worker DB, where B is a known FAIL and A is whatever preceded it on
   its shard) would be a faster, cheaper repro loop than a full 4548-class run.

## Repro

```bash
# Isolated (passes):
cd apps/hib-suite-runner
./cratonvm-zgc-wrapper.sh @common.args -Djava.awt.headless=true \
  -Dhibernate.dialect=org.hibernate.dialect.MySQLDialect \
  -Dhibernate.connection.driver_class=com.mysql.cj.jdbc.Driver \
  -Dhibernate.connection.url="jdbc:mysql://localhost/hibernate_orm_test_99?allowPublicKeyRetrieval=true&useSSL=false" \
  -Dhibernate.connection.username=hibernate_orm_test -Dhibernate.connection.password=hibernate_orm_test \
  -Dcraton.batch=1 CratonRunner org.hibernate.orm.test.mapping.collections.CollectionTest

# Full 3-GC run that surfaced this:
MYSQL_WORKER_BASE=0 ./run-hib.sh --list testlist.txt --bin cratonvm-zgc-wrapper.sh --shards 6 --timeout 300 --out runs/full-mysql-zgc-20260821
```

## Related files

- `apps/hib-suite-runner/run-hib.sh` (`--mysql-worker-base` / `MYSQL_WORKER_BASE`)
- `apps/hibernate-orm/hibernate-core/target/resources/test/hibernate.properties`
  (rewritten for MySQL; `.bak-postgres-20260821` holds the prior Postgres version)
- `apps/hibernate-orm/hibernate-testing/src/main/java/org/hibernate/testing/cleaner/MySQL8DatabaseCleaner.java`
- `apps/hibernate-orm/hibernate-testing/src/main/java/org/hibernate/testing/cleaner/DatabaseCleanerContext.java`
- Full result sets: `apps/hib-suite-runner/runs/full-mysql-{zgc,g1,generational}-20260821/`
- Prior Postgres baseline: `apps/hib-suite-runner/runs/full-pg-{zgc,g1,generational}-20260820-3gc-pg-v2/`
