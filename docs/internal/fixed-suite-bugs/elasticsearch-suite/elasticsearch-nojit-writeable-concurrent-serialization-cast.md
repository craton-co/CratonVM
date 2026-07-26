# Elasticsearch no-JIT concurrent serialization casts Object to Writeable

Status: open

Date observed: 2026-07-02

## Summary

In CratonVM no-JIT mode, `MappingStatsTests.testConcurrentSerialization` fails
with a bad cast from `Object` to Elasticsearch `Writeable`. The same class
passes HotSpot. The JIT-on CratonVM run fails the same class for a different
reason (`RandomizedContext.getPerThread()` null), so this no-JIT result adds a
second bug signature for the class.

Failure signature:

```text
java.util.concurrent.ExecutionException:
java.lang.ClassCastException: java.lang.Object cannot be cast to
org.elasticsearch.common.io.stream.Writeable

Caused by: java.lang.ClassCastException:
java.lang.Object cannot be cast to org.elasticsearch.common.io.stream.Writeable
```

## Evidence from no-JIT partial run

Run `es-nojit-full-20260702` was stopped by request after 1366 recorded
classes.

```text
index=330
module=server
class=org.elasticsearch.action.admin.cluster.stats.MappingStatsTests
CratonVM no-JIT=FAIL, 39.270s
HotSpot=PASS, 15.924s
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit off -Start 330 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-nojit-writeable-cast-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\target\release\cratonvm-elasticsearch-nojit-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\logs\server.org.elasticsearch.action.admin.cluster.stats.MappingStatsTests.out.log
```
