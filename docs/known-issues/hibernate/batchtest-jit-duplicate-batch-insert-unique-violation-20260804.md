# `batch.BatchTest` — JIT-only unique-constraint violation on `DataPoint(xval,yval)`, not the historical timeout

**Status:** OPEN (2026-08-04). New CratonVM bug, genuinely different symptom
from every prior `BatchTest` finding. Confirmed 100% reproducible under the
default JIT-on configuration, confirmed 0% reproducible (for the 3 small-`N`
methods) under `--nojit` — this is a JIT-specific correctness defect in the
JDBC batch insert/update path, not the previously-tracked "generic
interpreter/JDBC throughput margin" that `BatchTest` was filed under in
`../../internal/fixed-suite-bugs/hibernate/hib-120s-junit-timeout-cluster-20260716.md`'s
2026-07-30/31 recurrence section and `README.md`'s "same 'timeout-marginal
class + contended host' pattern" bullet.

## Symptom, today's fresh run

`apps/hib-suite-runner/runs/run-20260804-113511-custom/on-real/shard-2/raw.log`
(dev tip `a43a74ded`, `CratonVM-hib-local-0712-v3`), real-JDK, JIT on,
default flags:

```
@@RESULT org.hibernate.orm.test.batch.BatchTest found=4 started=4 ok=0 failed=4 aborted=0 skipped=0 ms=92671
```

All 4 `@Test` methods fail — **not** with a `TimeoutException` (the historical
signature for this class) but with a genuine
`org.hibernate.exception.ConstraintViolationException`:

```
@@TESTFAIL org.hibernate.orm.test.batch.BatchTest testBatchInsertUpdateSizeEqJdbcBatchSize(SessionFactoryScope) FAILED
org.hibernate.exception.ConstraintViolationException: could not execute batch [Unique index or primary key violation: "PUBLIC.XY INDEX PUBLIC.XY_INDEX_9 ON PUBLIC.DATAPOINT(XVAL NULLS FIRST, YVAL NULLS FIRST) VALUES ( /* key:10 */ 0.90000000000000002220, 0.62160996827066440000)"; SQL statement:
update DataPoint set description=?,xval=?,yval=? where id=? [23505-240]] [update DataPoint set description=?,xval=?,yval=? where id=?]
	at org.h2.jdbc.JdbcPreparedStatement.executeBatch(JdbcPreparedStatement.java:1277)
	at org.hibernate.engine.jdbc.batch.internal.SingleStatementBatchImpl.performExecution(SingleStatementBatchImpl.java:179)
	at org.hibernate.orm.test.batch.BatchTest.lambda$doBatchInsertUpdate$1(BatchTest.java:93)
```

The other 3 methods fail the same way, with the same colliding value
(`key:3` / `x=0.2, y=0.98006657784124160000`), some on the `update` flush,
one (`testBatchInsertUpdateSizeLtJdbcBatchSize`) on the plain `insert`.

## Test shape

`BatchTest.java` (`hibernate-core/src/test/java/org/hibernate/orm/test/batch/BatchTest.java`)
is `@SessionFactory`-scoped **once for the whole class** (all 4 `@Test`
methods share one `SessionFactory`/schema). Each method calls
`doBatchInsertUpdate(nEntities, nBeforeFlush, scope)`, which:

1. Inserts `nEntities` `DataPoint` rows with `x = i*0.1`, `y = cos(x)`
   (`i = 0..nEntities-1`, so `x` is unique per row by construction —
   `i*0.1` never repeats for distinct integer `i`), flushing/clearing every
   `nBeforeFlush` rows.
2. Scrolls all rows ordered by `x`, sets `description = "done!"` on each
   (an unconditional dirty-check re-issues `update ... set
   description=?,xval=?,yval=? where id=?` with the row's *unchanged* x/y),
   flushing every `nBeforeFlush` rows.
3. Scrolls and deletes every row, flushing every `nBeforeFlush` rows.

`DataPoint(xval,yval)` has a real unique index (`XY_INDEX_9`). Because `x`
is unique per inserted row by construction, and step 2 never changes x/y,
**the only way this constraint can fire is if two rows in the table
genuinely carry the identical `(xval,yval)` pair at flush time** — either a
duplicated row (the same logical insert executed twice) or a batched
`update`/`insert` whose bound `xval`/`yval` parameters for one row were
contaminated with another row's values.

## Root cause is JIT-only: `--nojit` control

Solo repro, same binary, same flags, `--nojit` added:

```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 cratonvm.exe --nojit --java-home "<jdk25>" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner org.hibernate.orm.test.batch.BatchTest
```

```
@@RESULT org.hibernate.orm.test.batch.BatchTest found=4 started=4 ok=3 failed=1 aborted=0 skipped=0 ms=291794
```

The 3 small (`N=50`) methods (`SizeEq`/`SizeLt`/`SizeGt`) **pass cleanly —
zero `ConstraintViolationException` anywhere in the log.** The 4th,
`testBatchInsertUpdate` (`N=5000`, no unique-violation risk changed — same
mechanism, just slower), instead hits the plain Hibernate-internal 120s
`@Timeout` (`TimeoutException`), which is the class's own source comment
("26 secs with batch flush" on HotSpot) being simply slower under a pure
interpreter — expected, unrelated, and consistent with this class's
long-tracked throughput-margin history.

Solo repro, JIT on (default), same binary:

```
@@RESULT org.hibernate.orm.test.batch.BatchTest found=4 started=4 ok=0 failed=4 aborted=0 skipped=0 ms=80300
```

100% (4/4), same `ConstraintViolationException` signature as the fresh suite
run. **Zero unique-violations under `--nojit`, 100% under JIT** — this is a
JIT-specific defect in the batched-statement/parameter-binding path, not a
generic throughput issue and not present in the interpreter at all.

## Which method is the true root, and why all 4 fail

`testBatchInsertUpdateSizeEqJdbcBatchSize` (batch size 20, `nBeforeFlush =
20`) fails **first**, on a completely fresh, just-created table (no other
test in the class has touched `DataPoint` yet) — its own update-phase batch
flush (covering rows 0-19, the first `nBeforeFlush`-sized batch) produces a
unique-index collision on `key:10` (`i=9`, `x=0.9, y=cos(0.9)=0.6216...`)
against another row that must already carry the identical `(x,y)` pair.
Since this is the **first** method to touch the table, this failure is
independent of test ordering or prior-test contamination — it is the real
defect.

Because the `ConstraintViolationException` aborts
`doBatchInsertUpdate`'s enclosing transaction before step 3 (the
delete-everything cleanup) ever runs, the offending method's 50 rows are
**left behind** in the shared, `@SessionFactory`-class-scoped, named
in-memory H2 database (`jdbc:h2:mem:db1;DB_CLOSE_DELAY=-1`, filtered from
`hibernate-core/target/resources/test/hibernate.properties` — the whole
class's 4 methods share this DB, not just the `SessionFactory`). Every
subsequent method in the class then inserts a **fresh** set of `i*0.1`-keyed
rows into a table that already has the first method's uncleaned leftovers,
so its own insert (or a later update flush) collides too — this is why all
4 methods fail, but only the first is evidence of the actual defect; the
other 3 are downstream contamination of the same uncleaned table, not
independent bugs.

## What's NOT yet pinned down

Two mechanisms are consistent with "two rows end up with the identical
`(xval,yval)`" and this session did not distinguish them:

1. **A spurious duplicate `INSERT`** — some batch of the first insert phase
   (rows 0-19, flushed at `i=19`) is executed twice against the JDBC driver,
   producing two physical rows for the same logical entity.
2. **Stale/cross-row parameter binding inside a batched `PreparedStatement`**
   — `SingleStatementBatchImpl`'s `addToBatch`/`performExecution` sequence
   (real `org.h2.jdbc.JdbcPreparedStatement`, interpreted/JIT-compiled
   bytecode) binds one row's `xval`/`yval` parameters but a later
   `addBatch()` call in the same batch fails to fully overwrite them, so a
   later row's batch entry silently carries an earlier row's x/y values —
   producing an accidental collision with whichever row legitimately owns
   that value, without any row being inserted twice.

   `native-builtins/src/apps_h2.rs` has no `addBatch`/`executeBatch`/
   `clearBatch` override, confirming this runs as real H2 bytecode, not a
   CratonVM shortcut. `native-builtins/src/phases_late/jdbc.rs` *does*
   register natives with those exact names, but on `java/sql/PreparedStatement`
   for CratonVM's own rusqlite-backed synthetic JDBC provider
   (`register_p68_jdbc`) — the function's own doc comment confirms it is
   reached only via `register_synthetic_overrides`,
   `#[cfg(feature = "synthetic-jdk")]`-gated and never registered on the
   real-JDK path this suite run uses (consistent with the stack traces above
   showing real `org.h2.jdbc.*` classes, and with the same
   real-vs-synthetic-registration split already documented for `Boolean` in
   `HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md`). So the
   defect, whichever of the two hypotheses it is, lives in the JIT's
   handling of ordinary bytecode (H2's `JdbcPreparedStatement`, or the
   surrounding parameter-marshaling/dirty-check machinery), not in a
   CratonVM native shortcut.

Both point at the same place (the JIT-compiled path through Hibernate's
`SingleStatementBatchImpl`/H2's batched-statement bytecode), but which one
is actually happening is unconfirmed. `key:10`'s failure lands on the
**update** flush (not the initial insert), which is mildly more consistent
with hypothesis 2 (a batched `update`, not `insert`, is where the collision
first appears), but this is not conclusive.

## Not a reopening of HIB-CV-38/39 or the array-header-corruption OOM

Checked against the two existing `BatchTest`-adjacent docs before filing
this: `HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md` and
`jit-inline-alloc-array-header-corruption-hibernate-batch.md` both describe
`DynamicBatchFetchTest` (a different class), and their symptoms — a
`Boolean.FALSE`-is-null NPE at JUnit-launcher bootstrap, and a `GC:
inconsistent header ... kind=Object but array_length=N` corruption storm
ending in `OutOfMemoryError` — are absent from every failure in today's
`BatchTest` log. This is a different bug.

## Repro

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 cratonvm.exe --java-home "<jdk25>" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner org.hibernate.orm.test.batch.BatchTest
```

Reproduced 100% (4/4 failures) across 2 separate JIT-on runs this session
(the fresh suite run plus one solo repro). `--nojit` reproduced 0/3 on the
same 3 small methods, 1 unrelated plain timeout on the 4th (`N=5000`) — see
above.

## Cross-reference

`README.md`'s "batch.BatchTest and batchfetch.DynamicBatchFetchTest — same
'timeout-marginal class + contended host' pattern" bullet and
`hib-120s-junit-timeout-cluster-20260716.md`'s 2026-07-30/31 recurrence
section both characterize `BatchTest` purely as a load-dependent
`TimeoutException` case; today's run shows that characterization no longer
covers the class's current failure mode (a genuine, JIT-only, 100%
deterministic `ConstraintViolationException`, not a timeout at all). Both
docs have a dated correction pointing here.
