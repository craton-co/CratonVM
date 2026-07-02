# Elasticsearch vector scorers return zero or wrong scores

Status: open

Date observed: 2026-07-02

## Summary

Several vector codec and vector query tests produce incorrect scores or vector
values under CratonVM. HotSpot passes the same representative classes.

Common signatures:

```text
java.lang.AssertionError: expected:<1.0> but was:<0.0>
java.lang.AssertionError: expected:<0.3569802> but was:<0.0>
java.lang.AssertionError: expected:<0.5> but was:<0.071428575>
```

Many of the same classes also hit the `SegmentVarHandle` foreign-memory issue,
but the zero-score assertions are tracked separately because some failures are
pure result mismatches.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 14 CratonVM-only failures containing vector score or
value mismatches.

Representative row:

```text
index=1427
module=server
class=org.elasticsearch.index.codec.vectors.es93.ES93HnswVectorsFormatTests
CratonVM=FAIL, 271.248s
HotSpot=PASS, 63.802s
```

Other examples:

```text
org.elasticsearch.index.codec.vectors.es93.ES93FlatBFloat16VectorFormatTests
org.elasticsearch.index.codec.vectors.es94.ES94HnswScalarQuantizedVectorsFormatTests
org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests
org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatVectorQueryTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1427 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-vector-score-zero-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.vectors.es93.ES93HnswVectorsFormatTests.out.log
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests.out.log
```
