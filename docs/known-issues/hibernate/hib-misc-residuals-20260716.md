# Misc non-passed residuals — 2026-07-16 full-suite rerun

The remaining 13 non-passed classes (of 20 total) not covered by the
[120-second timeout cluster](hib-120s-junit-timeout-cluster-20260716.md).
Source: full 4548-class rerun, real-JDK, JIT-on, `dev@2f02e939d`,
`TIMEOUT=1200`, local Windows host.

## `DefaultCatalogAndSchemaTest` — HANG (rc=124)

`org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`

**Status:** OPEN, needs isolated re-verification. This is the exact class
from an earlier this-session cluster that was run 4 times identically and
produced PASS/PASS/HANG/CRASH — originally mis-hypothesized as a
"global-temp-table race," later refuted with the true root cause identified
as the JIT guarded-inline-getfield fast path corrupting `getfield` results
(fixed by `93b33576`, flipping `guarded_inline_getfield_enabled()` to
opt-in default-off). Seeing this class HANG again in a fresh run suggests
either a distinct, still-open flakiness source, or that the class remains
inherently non-deterministic under some other condition not yet identified.
Needs several solo reruns (`--nojit` and JIT-on) to determine whether this
is reproducible or another one-off flake.

## `JarVisitorTest` — CRASH, rc=0, ms=0

`org.hibernate.orm.test.bootstrap.scanning.JarVisitorTest`

**Status:** Needs re-verification before treating as a real crash. The
result row is `process-died rc=0 ms=0` — an exit code of 0 with zero
elapsed time is the known harness-artifact shape previously seen during
mid-run disk-space exhaustion (`LOADERR`-family artifacts), not a genuine
non-zero-exit crash. Given this run coincided with a disk-space incident on
this same local host (see the [known-issues README](README.md) data-loss
note), this is suspected to be a harness/environment artifact rather than a
real CratonVM defect, but has not yet been confirmed via solo rerun.
`ScannerTest`, a sibling in the same `bootstrap.scanning` package previously
associated with a now-fixed "loader-blind jar scanning" bug, shows a
genuine 120s-timeout FAIL in this same run (tracked in the timeout cluster
doc) rather than the old `orm.xml doesn't exist` symptom — consistent with
that older bug staying fixed.

## `LockTest` — real timing-sensitive assertion failure

`org.hibernate.orm.test.jpa.lock.LockTest`

```
org.opentest4j.AssertionFailedError: execution exceeded timeout of 5000 ms by 2180 ms
```

**Status:** OPEN. 23 found / 14 ok / 1 failed / 8 skipped. Similar shape to
the 120s-timeout cluster (a real completion that's too slow) but against a
much tighter 5-second internal timeout specific to a lock-acquisition test,
so plausibly the same systemic throughput gap manifesting on a
tighter-margin test rather than a separate cause.

## `CriteriaBuilderNonStandardFunctionsTest` — real constraint violation

`org.hibernate.orm.test.query.criteria.CriteriaBuilderNonStandardFunctionsTest`

```
org.hibernate.exception.ConstraintViolationException: could not execute batch
[Unique index or primary key violation: "PUBLIC.CONSTRAINT_35E3F7 PRIMARY KEY ON ...
```

**Status:** OPEN, not a timeout — a genuine wrong-behavior symptom (20
found / 17 ok / 1 failed / 2 skipped). Worth checking whether this is
test-order dependent (a prior test in the same run leaving unexpected state
that collides on a primary key) versus a real id-generation double-issue
bug. Not yet root-caused.

## Already-expected ABORTED entries (matches HotSpot, not a defect)

Per [hib-bytecode-enhancement-loader-faithful-linking.md](../../internal/fixed-suite-bugs/hib-bytecode-enhancement-loader-faithful-linking-FIXED.md)
and the 2026-07-11 audit, these are expected `@CustomEnhancementContext`/
dialect-gated partial skips:

- `org.hibernate.orm.test.bytecode.enhancement.basic.InheritedTest`
- `org.hibernate.orm.test.bytecode.enhancement.basic.MappedSuperclassTest`
- `org.hibernate.orm.test.type.temporal.InstantTests`
- `org.hibernate.orm.test.type.temporal.LocalDateTimeTest`
- `org.hibernate.orm.test.type.temporal.OffsetDateTimeTest`
- `org.hibernate.orm.test.type.temporal.OffsetTimeTest`
- `org.hibernate.orm.test.manytomanyassociationclass.surrogateid.generated.ManyToManyAssociationClassGeneratedIdTest`
  (2026-07-16, confirmed) — 6 found / 3 ok / 3 aborted / 0 skipped. Root
  cause found via a custom `TestExecutionListener` (`AbortTraceRunner`,
  mirrors `CratonRunner`'s launcher setup but also captures
  `TestExecutionResult.getThrowable()` for ABORTED results, since
  `SummaryGeneratingListener.getFailures()` only covers FAILED). The 3
  aborted methods (`testRemoveAndAddEqualElement`,
  `testRemoveAndAddEqualCollection`, `testRemoveAndAddEqualElementNonKeyModified`
  — the three overridden in this subclass) all call
  `skipForGraphQueue(scope)`, which does
  `assumeFalse(getConfiguredQueueType() == QueueType.GRAPH, ...)`. Hibernate
  ORM 8's `QueueType.fromSetting(null)` defaults to `GRAPH`
  (`hibernate.flush.queue.type` is unset by the harness), so this assumption
  is expected to fail and skip these 3 legacy-ordering-specific methods by
  design, independent of the JVM. Confirmed by running the identical class
  through a standalone JUnit5 launcher directly on
  `/home/victor/jdk25/bin/java` (real HotSpot, same classpath as
  `common.args`): also 6 found / 3 ok / 3 aborted, same 3 methods, byte-for-byte
  identical abort message on both VMs:
  `org.opentest4j.TestAbortedException: Assumption failed: Legacy
  insert-before-delete ordering is not expected with the graph action queue`.
  Not a CratonVM defect — same shape as the other entries in this list, just
  gated by a Hibernate-internal default rather than `@CustomEnhancementContext`
  or a dialect check. No fix needed.

One exception: `org.hibernate.orm.test.type.temporal.ZonedDateTimeTest`
shows a captured signature this run —
`java.lang.InternalError: java.lang.CloneNotSupportedException` — on top of
the usual partial-abort shape (608 found / 404 ok / 204 aborted). Worth a
quick look to confirm this is the same expected-skip mechanism surfacing a
different message, versus a distinct new issue riding along with the
expected skips.
