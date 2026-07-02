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
