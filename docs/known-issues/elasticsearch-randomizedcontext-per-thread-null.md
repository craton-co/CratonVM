# Elasticsearch RandomizedContext per-thread state is null

Status: open

Date observed: 2026-07-02

## Summary

Several Elasticsearch randomized tests fail under CratonVM because
`RandomizedContext.getPerThread()` unexpectedly returns null after tests have
already run. HotSpot passes the same classes.

Failure signature:

```text
java.lang.NullPointerException: Cannot read field "randomnesses" because the
return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null
```

This points at thread-local or per-thread state lifetime handling in CratonVM.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 6 CratonVM-only failures with this signature.

Representative row:

```text
index=334
module=server
class=org.elasticsearch.action.admin.cluster.stats.MappingStatsTests
CratonVM=FAIL, 58.011s
HotSpot=PASS, 15.924s
```

Other examples:

```text
org.elasticsearch.index.codec.vectors.es93.ES93BinaryQuantizedVectorsFormatTests
org.elasticsearch.index.codec.vectors.es816.ES816HnswBinaryQuantizedVectorsFormatTests
org.elasticsearch.index.codec.vectors.es94.ES94ScalarQuantizedVectorsFormatTests
org.elasticsearch.lucene.queries.InetAddressRandomBinaryDocValuesRangeQueryTests
org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests
```

### Related symptom: duplicate `createTempDir()` paths → node-lock cascade

After the JDK-NIO `AbstractMethodError`s were fixed (see
`docs/internal/elasticsearch-jdk-nio-no-code-attribute.md`),
`InternalEngineFieldInfoCachingTests` and `NoOpEngineTests` still fail
deterministically (no stale lock files involved) with:

```text
java.lang.IllegalStateException: failed to obtain node locks, tried [X, X]
Caused by: org.apache.lucene.store.LockObtainFailedException: Lock held by this virtual machine
```

`ESTestCase.tmpPaths()` calls `createTempDir()` 1-3 times
(`TestUtil.nextInt(random(), 1, 3)`) to build `path.data`; in this run it
returned the SAME path twice instead of two distinct temp directories, so
`NodeEnvironment` tries to lock the same physical directory twice in one
process. `createTempDir()`'s naming is per-thread/RandomizedContext-scoped
state, so this is very likely the same underlying per-thread-state defect
tracked by this doc, manifesting as silent name collision rather than an
outright NPE. Not yet root-caused independently — flagging here rather than
opening a duplicate doc.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 334 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-randomizedcontext-perthread-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.action.admin.cluster.stats.MappingStatsTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
