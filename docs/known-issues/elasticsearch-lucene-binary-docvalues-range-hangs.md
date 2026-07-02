# Elasticsearch Lucene binary doc-values range query hangs

Status: open

Date observed: 2026-07-02

## Summary

Two Lucene binary doc-values range query tests hang under CratonVM until the
suite runner kills the process at the requested 300-second timeout. HotSpot
passes the same classes.

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
