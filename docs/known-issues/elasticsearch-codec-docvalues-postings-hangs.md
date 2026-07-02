# Elasticsearch codec/doc-values/postings hangs

Status: open

Date observed: 2026-07-02

## Summary

Several Elasticsearch codec, doc-values, and postings tests hang under
CratonVM until the suite runner kills the process at the requested 300-second
hang timeout. HotSpot passes most of the same classes.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- 7 CratonVM `HANG` rows in this family.
- 6 are CratonVM-only: HotSpot passed the same classes.
- 1 overlaps a HotSpot baseline failure.

Representative row:

```text
index=1352
module=server
class=org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests
CratonVM=HANG, 300.066s
HotSpot=PASS, 54.417s
```

Affected classes:

```text
org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests
org.elasticsearch.index.codec.bloomfilter.ES87BloomFilterPostingsFormatTests
org.elasticsearch.index.codec.postings.ES812PostingsFormatTests
org.elasticsearch.index.codec.tsdb.DocValuesForUtilTests
org.elasticsearch.index.codec.tsdb.es819.ES819TSDBDocValuesFormatVariableSkipIntervalTests
org.elasticsearch.index.codec.tsdb.ES87TSDBDocValuesFormatTests
org.elasticsearch.index.codec.tsdb.ES87TSDBDocValuesFormatVariableSkipIntervalTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1352 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-codec-docvalues-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.bloomfilter.ES85BloomFilterPostingsFormatTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
