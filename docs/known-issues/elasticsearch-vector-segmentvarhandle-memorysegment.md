# Elasticsearch vector codecs missing SegmentVarHandle MemorySegment access

Status: open

Date observed: 2026-07-02

## Summary

Vector codec and vector query tests fail under CratonVM when code reaches JDK
foreign-memory var-handle accessors. HotSpot passes the same representative
classes.

Observed signatures:

```text
java.lang.NoSuchMethodError:
java/lang/invoke/SegmentVarHandle.set(Ljava/lang/foreign/MemorySegment;JI)V
```

```text
java.lang.NoSuchMethodError:
java/lang/invoke/SegmentVarHandle.get(Ljava/lang/foreign/MemorySegment;J)B
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 25 CratonVM-only failures containing
`SegmentVarHandle` foreign-memory access failures.

Representative row:

```text
index=1341
module=server
class=org.elasticsearch.index.codec.vectors.diskbbq.ES920DiskBBQBFloat16VectorsFormatTests
CratonVM=FAIL, 38.684s
HotSpot=PASS, 18.334s
```

Other examples:

```text
org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940DiskBBQBFloat16VectorsFormatTests
org.elasticsearch.index.codec.vectors.es818.ES818BinaryQuantizedVectorsFormatTests
org.elasticsearch.index.codec.vectors.es93.ES93HnswBinaryQuantizedBFloat16VectorsFormatTests
org.elasticsearch.index.codec.vectors.es94.ES94HnswScalarQuantizedVectorsFormatTests
org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1341 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-vector-segmentvarhandle-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.vectors.diskbbq.ES920DiskBBQBFloat16VectorsFormatTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```

## No-JIT partial evidence

Run `es-nojit-full-20260702` was stopped by request after 1366 recorded
classes. The partial no-JIT run had already found 10 CratonVM failures where
the terminal failure was `SegmentVarHandle` foreign-memory access. All 10 were
HotSpot PASS classes. Additional TSDB hang logs also contain
`SegmentVarHandle` failures before the runner timeout.

Representative no-JIT row:

```text
index=1339
module=server
class=org.elasticsearch.index.codec.vectors.diskbbq.ES920DiskBBQBFloat16VectorsFormatTests
CratonVM no-JIT=FAIL, 37.964s
HotSpot=PASS, 18.334s
```

Evidence:

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\logs\server.org.elasticsearch.index.codec.vectors.diskbbq.ES920DiskBB.5aed92a84366.out.log
```
