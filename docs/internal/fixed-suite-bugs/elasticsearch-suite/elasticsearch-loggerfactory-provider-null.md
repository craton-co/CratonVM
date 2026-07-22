# Elasticsearch LoggerFactory provider null during vectorization init

**Status:** RESOLVED — not a CratonVM bug; fixed in the test fixture.
Date observed: 2026-07-02. Date resolved: 2026-07-02.

## Original symptom

`PreconditionerTests` (and `BinaryQuantizationTests`) failed under CratonVM
with:

```text
java.lang.ExceptionInInitializerError
Caused by: java.lang.NullPointerException: Cannot invoke
"org.elasticsearch.logging.internal.spi.LoggerFactory.getLogger(java.lang.Class)"
because the return value of
"org.elasticsearch.logging.internal.spi.LoggerFactory.provider()" is null
```

failing at `org.elasticsearch.simdvec.ESVectorizationProvider.<clinit>`,
reached transitively when the test body first touches vector-quantization
code.

## Why it looked like a CratonVM bug (and isn't)

`org.elasticsearch.logging.internal.spi.LoggerFactory` is **not** a
ServiceLoader/JPMS-SPI class despite the package name — it is a plain static
holder:

```java
private static volatile LoggerFactory INSTANCE;
public static LoggerFactory provider() { return INSTANCE; }
public static void setInstance(LoggerFactory INSTANCE) { LoggerFactory.INSTANCE = INSTANCE; }
```

`INSTANCE` is populated by exactly one call site,
`LogConfigurator.configureESLogging()`
(`server/src/main/java/org/elasticsearch/common/logging/LogConfigurator.java:148-150`).
Production reaches it via `Elasticsearch.initPhase1()`; most unit tests reach
it via `ESTestCase`'s static initializer.

`PreconditionerTests` and `BinaryQuantizationTests`
(`server/src/test/java/org/elasticsearch/index/codec/vectors/{diskbbq,es816}/`)
extend Lucene's `LuceneTestCase` **directly**, not `ESTestCase`, so nothing
ever calls `configureESLogging()` before the test body exercises vector code
that logs through `ESVectorizationProvider`. A sibling test in the same
package, `ES816BinaryFlatVectorsScorerTests`, already carries the guard:

```java
static {
    LogConfigurator.configureESLogging(); // native access requires logging to be initialized
}
```

Confirming this is a fixture gap and not a VM defect: **HotSpot fails the
same test with the same signature** (see
`docs/known-issues/elasticsearch-nojit-partial-suite-bug-info.md`'s "2
LoggerFactory.provider() null log files in vectorization init" line, and the
HotSpot baseline row for index 1352/1360 in
`es-full-hotspot-20260702/hotspot-jit/results.tsv`, both `FAIL` with the
identical `ExceptionInInitializerError`). This class of "isolated harness
never ran ES bootstrap" false-positive was seen once before in
[`ES-FAIL-03-RETRACTED-nativeaccess-not-a-bug.md`](ES-FAIL-03-RETRACTED-nativeaccess-not-a-bug.md)'s
`NaProbe` aside.

## Fix

Added the same static-init guard `ES816BinaryFlatVectorsScorerTests` already
has to the two gap classes, in the local `apps/elasticsearch` checkout:

- `server/src/test/java/org/elasticsearch/index/codec/vectors/diskbbq/PreconditionerTests.java`
- `server/src/test/java/org/elasticsearch/index/codec/vectors/es816/BinaryQuantizationTests.java`

```java
static {
    LogConfigurator.configureESLogging(); // native access requires logging to be initialized
}
```

(`DocIdsWriterTests.java`, in the same `diskbbq` package, also lacks the
guard but does not touch vector-quantization code and was not observed to
fail — left untouched to keep the diff scoped to the reported failure.)

## Verification

Recompiled `:server:compileTestJava` and reran both classes against the
existing `es-nojit-full-20260702` binary
(`cratonvm-elasticsearch-nojit-suite-20260702.exe`), `--nojit`:

```text
PASS  89.5s  org.elasticsearch.index.codec.vectors.diskbbq.PreconditionerTests
PASS  14.2s  org.elasticsearch.index.codec.vectors.es816.BinaryQuantizationTests
```

Both previously failed with the `LoggerFactory.provider()` NPE at these same
indices.

## Evidence

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\logs\server.org.elasticsearch.index.codec.vectors.diskbbq.PreconditionerTests.{out,err}.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv  (rows 1352, 1360 — HotSpot FAIL, same signature)
```
