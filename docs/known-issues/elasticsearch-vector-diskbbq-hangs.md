# Elasticsearch vector and DiskBBQ hangs

Status: open

Date observed: 2026-07-02

## Summary

Vector codec and vector query tests hang under CratonVM until the suite runner
kills the process at the requested 300-second timeout. HotSpot passes the same
classes.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 7 CratonVM-only `HANG` rows in this family.

Representative row:

```text
index=1370
module=server
class=org.elasticsearch.index.codec.vectors.diskbbq.DocIdsWriterTests
CratonVM=HANG, 300.099s
HotSpot=PASS, 123.667s
```

Affected classes:

```text
org.elasticsearch.index.codec.vectors.diskbbq.DocIdsWriterTests
org.elasticsearch.index.codec.vectors.diskbbq.ES920DiskBBQVectorsFormatTests
org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940DiskBBQVectorsFormatTests
org.elasticsearch.index.codec.vectors.diskbbq.next.ESNextDiskBBQVectorsFormatTests
org.elasticsearch.index.codec.vectors.es93.ES93FlatVectorFormatTests
org.elasticsearch.index.codec.vectors.es93.ES93HnswBitVectorsFormatTests
org.elasticsearch.search.vectors.IVFKnnFloatSlicedVectorQueryTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1370 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-vector-diskbbq-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.vectors.diskbbq.DocIdsWriterTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
