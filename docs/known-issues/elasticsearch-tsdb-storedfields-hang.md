# Elasticsearch TSDB stored fields no-JIT hang

Status: open

Date observed: 2026-07-02

## Summary

`TSDBStoredFieldsFormatTests` hangs in CratonVM no-JIT mode until the suite
runner kills it at the configured 300-second timeout. HotSpot passes the same
class. In the CratonVM JIT-on run, this class failed rather than hanging, so
the no-JIT run exposes a more severe interpreter-mode symptom.

## Evidence from no-JIT partial run

Run `es-nojit-full-20260702` was stopped by request after 1366 recorded
classes.

```text
index=1344
module=server
class=org.elasticsearch.index.codec.storedfields.TSDBStoredFieldsFormatTests
CratonVM no-JIT=HANG, 300.012s
HotSpot=PASS
```

The stdout log only reached suite startup:

```text
JUnit version 4.13.2
The current build is a snapshot, feature flag [field_info_caching_directory] is enabled
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit off -Start 1344 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-tsdb-storedfields-nojit-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\target\release\cratonvm-elasticsearch-nojit-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\logs\server.org.elasticsearch.index.codec.storedfields.TSDBStoredFieldsFormatTests.out.log
```
