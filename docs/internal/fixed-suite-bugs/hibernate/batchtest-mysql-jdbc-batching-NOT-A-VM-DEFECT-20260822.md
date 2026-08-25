# `batch.BatchTest` — CratonVM is 4.5-7.2x slower than HotSpot on Hibernate's JDBC batch path, and the gap is NOT MySQL-specific

> **RETIRED to internal 2026-08-24.** This page's own Status records the
> outcome: root-caused, **no VM defect found**, and its MySQL-specific premise
> refuted by a direct cross-database control. It was never a known issue in the
> sense the `known-issues/` folder is for; it is a measurement record.

## Status
**ROOT-CAUSED (2026-08-22). No VM defect found, and the page's original
MySQL-specific premise is REFUTED by a direct cross-database control.** The
class is a real, reproducible throughput gap, but it is the general CratonVM
execution-engine gap on call-dense ORM/JDBC code, not a MySQL driver
interaction. It crosses the harness's 120 s per-method JUnit budget on MySQL
and not on Postgres only because Postgres is ~2.5x cheaper in absolute terms
for BOTH runtimes.

## Severity
**LOW as a bug (nothing is wrong), MEDIUM as a characteristic.** No wrong
answers, no crash. What this page is worth keeping for is the measurement
below: it says where the 4.5-7.2x actually is, so throughput work aims at the
right thing.

## Context (from the original page, unchanged)

Found while triaging the 50-class FAIL set from the 2026-08-21/22 Hibernate
ORM x MySQL 3-GC full-suite run (see
[`mysql-cross-class-stale-schema-shared-worker-db-20260822.md`](mysql-cross-class-stale-schema-shared-worker-db-20260822.md)
for the full triage). Of that 50, 46 turned out to be a cross-class
schema-reuse artifact and 3 more were pre-existing / not-CratonVM issues. This
was the one residual that isolated cleanly, and the original page read that
isolation as "a genuine, new, MySQL-specific CratonVM behavior". The first
measurement below is the control that reading needed and never had.

The original evidence, for the record: HotSpot (Temurin 25.0.3)
`found=4 ok=4 failed=0` in 24 816 ms for the whole class; CratonVM (ZGC, JIT
on) `found=4 ok=3 failed=1` in 140 786 ms, the failure being
`TimeoutException: testBatchInsertUpdate(...) timed out after 120 seconds`.
Both reproduce here.

## The control the original page never ran

Same class, same method, same harness, same `common.args`, fresh never-used
database per arm, one runtime pair per database, nothing else running on the
box. `test_ms` is the JUnit-reported time for `testBatchInsertUpdate` alone;
wall/CPU are the whole forked process, measured through
`System.Diagnostics.Process` (user + kernel).

| database | HotSpot `test_ms` | CratonVM `test_ms` | ratio |
|---|---:|---:|---:|
| MySQL 9.7 (`com.mysql.cj.jdbc.Driver`) | 26 676 | 118 874 | **4.46x** |
| PostgreSQL 16 (`org.postgresql.Driver`) | 10 625 | 76 314 | **7.18x** |

**The gap is WORSE on Postgres.** The original page inferred MySQL-specificity
from the class's absence in the 2026-08-20 Postgres 3-GC FAIL list. That
absence has a simpler explanation: the harness sets
`-Djunit.jupiter.execution.timeout.default=120s`, and CratonVM's Postgres
number (76 s) sits under that cliff while its MySQL number (119-141 s
depending on run) sits on top of it. Same characteristic, one threshold.

Process CPU for the MySQL pair, which also rules out I/O:

| runtime | wall | user CPU | kernel CPU |
|---|---:|---:|---:|
| HotSpot | 31.3 s | 22.5 s | 8.2 s |
| CratonVM | 127.1 s | 94.2 s | 15.2 s |

User CPU is 4.2x, kernel CPU 1.85x. The run is CPU-bound on both runtimes; the
database server is local and the round-trips cost both sides the same.
(HotSpot is in fact handicapped in this pair: log4j2 TRACE is live on the
HotSpot arm and not on the CratonVM arm — 16 MB of SQL log versus 1.8 MB — so
HotSpot does strictly *more* work for its 26.7 s.)

## Where the time is: per-primitive A/B

Microbenchmark run on both runtimes with the identical classpath
(`apps/hib-suite-runner/HibfixHotProbe.java`, `HibfixNativeProbe.java`),
nanoseconds per operation:

| operation | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| pure-arithmetic loop (calibration) | 287 | 711 | **2.5x** |
| `Enum.ordinal()` | 2 | 312 | **156x** |
| `Enum.name()` | 2 | 323 | **161x** |
| `Object.getClass()` | 4 | 419 | **105x** |
| `Character.isWhitespace` x38 chars | 27 | 2 431 | **90x** |
| `EnumSet.contains` (one call) | 1 | 519 | **519x** |
| `list.stream().map().forEach()` on an EMPTY list | 108 | 9 959 | **92x** |
| `StringInspector` scan of one INSERT statement | 844 | 1 517 705 | **1798x** |
| `OpenTelemetryHandler.startSpan` (Connector/J's own) | 1 309 | 20 867 | **16x** |

The calibration row is the floor: CratonVM's compiled arithmetic is 2.5x
HotSpot's. **Every row far above 2.5x is a trivial JDK primitive that CratonVM
serves from a registered Rust native rather than from bytecode**, and each such
call pays the native dispatch funnel (~300-500 ns) instead of the 1-4 ns
HotSpot spends inlining a field read. `EnumSet.contains` compounds two of them
(`Object.getClass` + `Enum.ordinal`), and `StringInspector` compounds
`EnumSet.contains` several times per character of SQL — which is how a 60-char
INSERT statement costs 1.5 ms to scan.

## Native-invocation census on the real workload

`--dump-native-registry` over the actual `testBatchInsertUpdate` run:
**14 775 100 native invocations** in 117 s. Top of the census:

```
2631717  java/lang/Enum.ordinal          <- 17.8% of all native calls, 4x the runner-up
 675854  java/lang/Character.digit
 549018  java/nio/ByteBuffer.array
 454584  java/lang/StringBuilder.append
 403796  jdk/internal/misc/Unsafe.compareAndSetInt
 339321  java/util/Arrays.copyOfRange
 325093  java/lang/Boolean.booleanValue
 306345  java/util/ArrayList$Itr.hasNext
 223772  java/lang/Character.isWhitespace
 187441  java/lang/Class.cast
 178477  java/lang/Class.isInstance
 152615  java/lang/Object.getClass
```

Read the multiplier and the population together, not separately: 14.8 M calls
at ~300 ns is ~4.4 s, i.e. **under 5% of the 117 s**. The per-call ratios above
are enormous and the aggregate here is small. Anyone reaching for the native
funnel as *this class's* fix should stop at this paragraph — the win is bounded
at a few percent.

## JIT coverage on the same run

`CRATONVM_DBG=jit-method-stats`:

```
1339 methods tracked, 964245 invocations | still-interpreted=66 c1=238 c2=1035
hot_but_stuck_in_interpreter=46 (ineligible-by-policy=21, compile-failures=7)
JIT skip-seal census: 2676 sealed before any compile | clinit=1410 calls-native-shadowed-method=1266
```

The hottest never-compiled methods are all on the per-statement driver path:

```
41153  sun/nio/ch/NioSocketImpl.implRead          compile-failed  rbc6-handler-reads-unsafe-local
31540  com/mysql/cj/otel/OpenTelemetryHandler.startSpan                 ineligible-by-policy
30964  com/mysql/cj/protocol/a/NativeProtocol.getValueEncoderSupplier   ineligible-by-policy
20532  com/mysql/cj/otel/OpenTelemetryHandler.propagateContext          ineligible-by-policy
20468  com/mysql/cj/jdbc/CloseOption.in                                 ineligible-by-policy
20094  sun/nio/ch/NioSocketImpl.endWrite          compile-failed  rbc6-handler-reads-unsafe-local
15028  com/mysql/cj/NativeQueryAttributesBindings.containsAttribute     ineligible-by-policy
10638  java/nio/charset/Charset.defaultCharset()  compile-failed  rbc6-handler-reads-unsafe-local
 5492  com/mysql/cj/jdbc/EscapeProcessor.escapeSQL compile-failed  rbc6-handler-reads-unsafe-local
```

`ineligible-by-policy` here is the `calls-native-shadowed-method` seal, and
**lifting it is measured NOT to help**: `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0`
takes the same method from 118.9 s to **175.2 s** and makes it FAIL (the seal is
a correctness guard, and compiling the 1266 extra methods costs more than it
returns). That agrees with the netty measurement already recorded on
`jit_invoke_targets_native_shadow`. Cross it off.

`rbc6-handler-reads-unsafe-local` is a genuine compiler limitation and is the
one open lead on this page: it denies four hot per-statement methods, including
`Charset.defaultCharset()` — a cached static getter called 10 638 times and
interpreted every one of them.

## Workload A/Bs

| arm | `test_ms` | vs baseline |
|---|---:|---|
| baseline (MySQL, ZGC, JIT on) | 118 874 | — |
| `&openTelemetry=DISABLED` in the JDBC URL | 100 074 | **-15.8%** |
| `&useServerPrepStmts=true` | 150 145 | +26%, and FAILS |
| `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0` | 175 150 | +47%, and FAILS |

Connector/J 9.7 calls `OpenTelemetryHandler.startSpan` + `propagateContext`
about twice per statement (31 540 + 20 532 calls here) and each one builds a
stream pipeline over an empty `linkTargets` list. That is ~16% of the run and
the single largest identified slice — it is *Connector/J's* behaviour, but
CratonVM pays 16x HotSpot for it because the empty-stream pipeline itself is
92x (see the table above). `useServerPrepStmts` and lifting the native-shadow
seal both make things worse; neither is a lever.

## What this page is NOT

- Not MySQL-specific (cross-database control above).
- Not `BigDecimal` — the `--stack-sample-ms` profile puts **1 sample out of
  4783** anywhere in `java/math/BigDecimal`, despite the test doing
  `new BigDecimal(double).setScale(19)` twice per row for 5000 rows.
- Not I/O — 22% of samples are parked on the socket, and process CPU accounts
  for 86% of wall on the CratonVM arm.
- Not the JDBC batch path in particular. Connector/J executes the batch
  serially without `rewriteBatchedStatements`, which is normal, and costs both
  runtimes the same round-trips.
- Not related to
  [`batchtest-jit-duplicate-batch-insert-unique-violation-20260804.md`](batchtest-jit-duplicate-batch-insert-unique-violation-20260804.md)
  beyond the class name — that was a fixed `Arrays.equals` correctness defect.

## Next steps

1. `rbc6-handler-reads-unsafe-local` — the only refusal on this page that is a
   compiler limitation rather than a measured-neutral policy. Four hot methods,
   one of them a cached static getter.
2. Do NOT chase the native funnel for this class: the census says it is worth
   at most ~4% here. It is worth chasing where a large per-call ratio meets a
   large population (the `EnumSet`/`Enum.ordinal` pair on any SQL-parsing
   workload is the shape that qualifies).
3. The 120 s per-method JUnit budget is the reporting cliff, not a VM fact. A
   run that reports this class FAIL and a run that reports it PASS can be 90 s
   apart in a workload whose spread is that wide; record `test_ms`, not just
   PASS/FAIL, when this class is used as a signal.

## Repro

```bash
cd apps/hib-suite-runner && RUNNER=MethodRunner ./hibfix-cpu.sh cv-cpu <cratonvm.exe> <fresh-mysql-db> -- org.hibernate.orm.test.batch.BatchTest testBatchInsertUpdate
```

```bash
cd apps/hib-suite-runner && RUNNER=MethodRunner ./hibfix-pg.sh cv-pg <cratonvm.exe> hibernate_orm_test -- org.hibernate.orm.test.batch.BatchTest testBatchInsertUpdate
```

```bash
cd apps/hib-suite-runner && RUNNER=HibfixNativeProbe ./hibfix-cpu.sh nprobe-cv <cratonvm.exe> NONE --
```

The Postgres control arm needs a container: `docker run -d --name hibfix-pg -e POSTGRES_USER=hibernate_orm_test -e POSTGRES_PASSWORD=hibernate_orm_test -e POSTGRES_DB=hibernate_orm_test -p 5433:5432 postgres:16`.

## Related files

- `apps/hibernate-orm/hibernate-core/src/test/java/org/hibernate/orm/test/batch/BatchTest.java`
- `apps/hib-suite-runner/HibfixHotProbe.java`, `apps/hib-suite-runner/HibfixNativeProbe.java`
- `apps/hib-suite-runner/hibfix-cpu.sh`, `hibfix-cpu.ps1`, `hibfix-pg.sh`
- [`mysql-cross-class-stale-schema-shared-worker-db-20260822.md`](mysql-cross-class-stale-schema-shared-worker-db-20260822.md)
