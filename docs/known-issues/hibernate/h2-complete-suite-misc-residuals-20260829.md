# H2 complete-suite residuals — four unrelated single-class findings

## Status
New findings, 2026-08-29, H2 complete-suite run (4548 classes). Four distinct
mechanisms, grouped here only because each is a single class with no obvious
cluster-mate. None cross-checked against HotSpot yet. None root-caused.

## 1. `UniqueConstraintBatchingTest.testBatching` — expected 1, got 0

```
org.hibernate.orm.test.annotations.uniqueconstraint.UniqueConstraintBatchingTest.testBatching

org.opentest4j.AssertionFailedError: expected: <1> but was: <0>
```

No detail beyond the bare count mismatch captured yet. Needs the test source
read to know what the `1` vs `0` represents (likely a constraint-violation
count or a batch-size count) before this can be characterized further.

## 2. `PackagedEntityManagerTest.testExcludeHbmPar` — missing sequence relation

```
org.hibernate.orm.test.bootstrap.scanning.PackagedEntityManagerTest.testExcludeHbmPar

org.hibernate.exception.SQLGrammarException: could not extract ResultSet
[ERROR: relation "caipirinha_seq" does not exist]
```

A generated-sequence table (`caipirinha_seq`) that schema export should have
created isn't present when the test queries it. Could be an H2-specific
schema-generation gap (sequence emulation differs from Postgres) or a real
CratonVM-side timing/ordering issue in when schema export runs relative to
this test's packaged-JAR scanning setup. Not distinguished.

## 3. `HqlParserMemoryUsageTest.testParserMemoryUsage` — parser used 2.4x its memory budget

```
org.hibernate.orm.test.hql.HqlParserMemoryUsageTest.testParserMemoryUsage

org.opentest4j.AssertionFailedError: Parsing of queries consumes too much memory
(630335 KB), when at most 256 MB are expected ==> expected: <true> but was: <false>
```

630 MB actual vs a 256 MB budget — 2.46x over. This is a memory-usage
assertion, not a crash or OOM, so the test ran to completion; CratonVM's HQL
parser (or something it retains alongside parsing, e.g. a cache that doesn't
evict) is using substantially more memory than HotSpot's for the same query
set. Worth comparing against HotSpot directly since this is a quantitative,
reproducible number, not a flake-shaped one.

## 4. `JpaLargeBlobTest.jpaBlobStream` — timed out after 120s

```
org.hibernate.orm.test.lob.JpaLargeBlobTest.jpaBlobStream

java.util.concurrent.TimeoutException: jpaBlobStream(...) timed out after 120 seconds
```

A blob-streaming test exceeding its 120s timeout — could be a genuine
throughput gap in CratonVM's I/O/streaming path for large BLOB data (this
session's memory notes record several "native call ~300ns floor" and
FFM-segment-access throughput gaps as a recurring category), or could be
H2-specific BLOB handling being slower than Postgres's for this test's data
size. Not measured or distinguished.

## Not yet done (all four)
- HotSpot-on-H2 A/B for each, to separate "H2-specific behavior" from
  "genuine CratonVM defect" — none of the four has this yet.
- Reading each test's source to understand exactly what's being asserted
  (especially #1's bare `<1>` vs `<0>`).
- For #3 specifically: a memory profile of the HQL parser to find what's
  actually retained, since 2.46x over budget is a large, reproducible,
  measurable gap worth chasing on its own if confirmed CratonVM-specific.

## Repro

```bash
cd apps/hib-suite-runner
cratonvm.exe --java-home <jdk25> -XX:+UseZGC <hibernate-orm classpath+args> \
  JUnitRunner org.hibernate.orm.test.annotations.uniqueconstraint.UniqueConstraintBatchingTest
  # substitute the other three class names as needed
```

hibernate.properties must point at H2 for these to reproduce as measured
here (the same suite on Postgres showed a completely different, much larger
FAIL set traced almost entirely to a `DbReset.java` harness gap — see
`complete-suite-postgres-931-fails-are-worker-db-reset-disabled-not-cratonvm-20260829.md`
— none of the four classes above were confirmed present in that noise-heavy
Postgres FAIL set specifically).
