# Elasticsearch Lucene binary doc-values range query hangs

Status: open (root cause #1 FIXED; root cause #2 open and now the sole blocker)

Date observed: 2026-07-02
Date updated: 2026-07-04

## Summary

Two Lucene binary doc-values range query tests hang under CratonVM until the
suite runner kills the process at the requested 300-second timeout. HotSpot
passes the same classes.

Root-caused to **two independent bugs** stacked in the same test run:

1. **FIXED** (branch `fix/es-binary-docvalues-range-hang`, merged to dev):
   CratonVM's JIT permanently blacklisted ANY method containing an
   `invokedynamic` instruction ANYWHERE in its bytecode, even on a dead code
   path — the classic case being `assert cond : "msg" + var;`, whose message
   string-concatenation compiles to `invokedynamic` behind a
   `$assertionsDisabled` guard. `org.apache.lucene.util.fst.NodeHash.add`,
   `NodeHash$PagedGrowableHash.nodesEqual`, and
   `FSTCompiler$UnCompiledNode.addArc`/`.replaceLast` — hot, per-FST-node
   methods exercised heavily while merging the term dictionary for
   `int_range_dv_field`/`long_range_dv_field` — all contain such an assert and
   were permanently stuck in the interpreter, a ~100-300x slowdown that alone
   was enough to blow the suite's 300s timeout for the Integer/Long range
   variants (Integer and Long share `RangeType.LONG`'s encode/query path,
   which produces far more distinct FST nodes than Float/Double, explaining
   why only these two classes hung). Fixed by making the JIT scanner accept
   `invokedynamic` and lowering it to an unconditional jump to the existing
   "uncommon trap" deopt stub (`DeoptReason::UnreachedCode`) instead of
   vetoing the whole method — see the commit on this branch for the full
   design and the standalone `FSTCompiler`/`NodeHash` stress repro used to
   isolate it without needing the ES harness.

2. **OPEN, now the sole blocker**: with #1 fixed, the same two test classes
   still do not complete within a reasonable window. The hang moved from FST
   construction to genuine concurrent search: `BaseRangeFieldQueryTestCase`'s
   `verify`/`testAllEqual` methods drive `IndexSearcher.search` via Lucene's
   `TaskExecutor`, and a worker thread blocks in `LRUQueryCache.putIfAbsent` →
   `ReentrantReadWriteLock$WriteLock.lock()` → `AbstractQueuedLongSynchronizer`
   contention, with severe (not necessarily infinite) slowness rather than a
   clean deadlock. This matches the already-documented limitation at
   `docs/book/src/java-support/limitations.md`: "`java.util.concurrent` is
   broad but not complete. Some constructs (full ForkJoin parity,
   `ReentrantReadWriteLock`, `Phaser`) are still being brought to full
   parity." Not yet investigated further — needs its own root-cause pass
   (possibly in the concurrent `IndexSearcher`/`LRUQueryCache` interaction
   specifically, since `ReentrantReadWriteLock` itself works correctly in
   isolation elsewhere).

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 2 CratonVM-only `HANG` rows in this family.

Representative row:

```text
index=2163
module=server
class=org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests
CratonVM=HANG, 300.200s
HotSpot=PASS, 15.336s
```

Affected classes:

```text
org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests
org.elasticsearch.lucene.queries.LongRandomBinaryDocValuesRangeQueryTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 2163 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-binary-docvalues-range-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
