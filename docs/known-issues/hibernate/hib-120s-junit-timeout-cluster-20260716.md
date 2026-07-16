# 120-second JUnit timeout cluster — 7 classes, one suspected systemic cause

| | |
|---|---|
| **Status** | OPEN — new finding, 2026-07-16 full-suite rerun. Not individually root-caused yet. |
| **Area** | Suspected: JIT/interpreter throughput, GC pause behavior, or native-call dispatch overhead under real-JDK+JIT-on mode. |
| **Severity** | Medium — no crashes or wrong results, but a real perf/timing gap wide enough to blow through Hibernate's own generous internal timeouts. |

## Symptom

Seven unrelated Hibernate test classes, spanning batching, JSON functions,
ID generation, insert ordering, boot scanning, SQL execution, and type
contribution, all fail with the identical shape:

```
java.util.concurrent.TimeoutException: <testMethod>(...) timed out after 120 seconds
```

This is Hibernate's own `@Timeout(120)`-style internal JUnit guard on
individual test methods — not the harness's outer `TIMEOUT=1200` (all of
these classes complete well under the harness timeout; the *test method
itself* reports having exceeded a 120-second internal watchdog).

## Affected classes (2026-07-16 rerun, real-JDK, JIT-on, `dev@2f02e939d`)

| Class | Method | Elapsed (ms) | Notes |
|---|---|---|---|
| `org.hibernate.orm.test.batch.BatchTest` | `testBatchInsertUpdate` | 320518 | |
| `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest` | `testMultiLoad` | 270408 | |
| `org.hibernate.orm.test.function.json.JsonArrayUnnestTest` | `testUnnest` | 523811 | |
| `org.hibernate.orm.test.id.uuid.rfc9562.UUidV6V7GeneratorTest` | `testMonotonicityUuid6` | 622550 | Contradicts the 2026-07-14 "PARTIALLY RESOLVED" note in the archived UUID doc — the quadratic-GC-cleanup fix improved `--nojit` timing (71-72s) but this JIT-on run still exceeds the 120s internal timeout by a wide margin. Residual, not a regression of the original fix. |
| `org.hibernate.orm.test.insertordering.InsertOrderingRCATest` | `testBatching` | 165898 | |
| `org.hibernate.orm.test.bootstrap.scanning.ScannerTest` | `testCustomScanner` | 134963 | |
| `org.hibernate.orm.test.sql.exec.SmokeTests` | `testQueryConcurrency` | 193477 | Contradicts the 2026-07-14 "all sql.exec.* classes pass" note in the archived assertion-longtail doc — this specific concurrency-flavored test within `SmokeTests` still times out; the other 16 tests in the class pass. |
| `org.hibernate.orm.test.type.contributor.LiteralRenderingTest` | `testIdVersionFunctions` | 347729 | |

(`org.hibernate.orm.test.jpa.lock.LockTest` has a related but distinct
symptom — `AssertionFailedError: execution exceeded timeout of 5000ms by
2180ms` on a much tighter 5-second internal timeout — tracked separately in
[hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md) since it may
be a different, more timing-sensitive class of issue.)

## Working hypothesis

All 7 failures are "test completes correctly but too slowly," not wrong
results — `ok` count is one less than `found` in every case, with the single
failed test being exactly the one that tripped its internal timeout. The
shared shape (a generous-but-finite internal timeout, tripped only under
CratonVM) suggests a single systemic throughput gap versus HotSpot — most
likely in JIT-compiled hot-path dispatch, GC pause frequency/duration, or
native-call overhead for JDBC/H2 round-trips — rather than 7 independent
bugs in unrelated Hibernate subsystems. `testQueryConcurrency` and
`testMonotonicityUuid6` in particular are the kind of test names that stress
concurrent/tight-loop execution, consistent with a throughput explanation
over a correctness one.

## Not yet done

- HotSpot timing comparison on the same 7 methods (same harness, same
  `TIMEOUT`, `HOTSPOT=1`) to quantify the actual CratonVM/HotSpot elapsed-time
  ratio and confirm this is CratonVM-specific rather than environmental
  (shared host contention was ruled out for prior clusters this session, but
  this is a fresh local-host run and hasn't been checked against a quiet
  baseline).
- Per-class profiling (`CRATONVM_DBG_JIT_DISASM`, GC pause counters) to
  narrow "systemic slowness" down to a specific subsystem.

## Related finding (2026-07-16): CriteriaBuilderNonStandardFunctionsTest joins this shape, root cause narrowed to JIT compile-time tax

Investigated as a separately-filed "real constraint violation" residual
(see [hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md)'s
CriteriaBuilderNonStandardFunctionsTest entry for the full writeup). Same
symptom shape as this cluster (prepareData(...) TimeoutException after
120s, ok = found - 1). Bisected via --nojit (3/3 clean) and via
JIT-nominally-on-but-thresholds-raised-so-compilation-never-triggers (2/2
clean) versus default JIT-on (0/6 clean immediately prior) -- both
bisections converge on JIT compilation-time tax in a short-lived process,
the same mechanism this file's sibling LockTest entry (in
hib-misc-residuals-20260716.md) found independently the same day. Live gdb
capture during one stall also caught a genuine, narrow, uncached
per-call class-hierarchy-walk allocation (native-collections/src/lib.rs's
al_slots_for -> classloading/src/class.rs's is_subclass_of) as a
contributing CPU-bound hot path, though not proven to be the dominant cost.
Worth folding into whichever session picks up this cluster's "per-class
profiling" next step -- the JIT-compile-tax bisection methodology (disable
JIT vs raise compile thresholds vs default) is a fast, cheap first cut that
could be run against the other 7 classes here before deeper profiling.
