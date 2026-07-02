# Elasticsearch LoggerFactory provider null during vectorization init

Status: open

Date observed: 2026-07-02

## Summary

During vectorization initialization, CratonVM can observe
`LoggerFactory.provider()` as null and fail class initialization. This appears
in both JIT-on and no-JIT evidence, but the clearest no-JIT repro is
`PreconditionerTests`.

Failure signature:

```text
java.lang.ExceptionInInitializerError
Caused by: java.lang.NullPointerException: Cannot invoke
"org.elasticsearch.logging.internal.spi.LoggerFactory.getLogger(java.lang.Class)"
because the return value of
"org.elasticsearch.logging.internal.spi.LoggerFactory.provider()" is null
```

## Evidence from no-JIT partial run

Run `es-nojit-full-20260702` was stopped by request after 1366 recorded
classes. The signature appeared in:

```text
index=1364
module=server
class=org.elasticsearch.index.codec.vectors.diskbbq.PreconditionerTests
CratonVM no-JIT=FAIL, 132.676s
HotSpot=FAIL
```

It also appears in `BinaryQuantizationTests`, which then reports
RandomizedTesting suite-timeout accounting.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit off -Start 1364 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-loggerfactory-provider-null-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\target\release\cratonvm-elasticsearch-nojit-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\logs\server.org.elasticsearch.index.codec.vectors.diskbbq.PreconditionerTests.out.log
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\logs\server.org.elasticsearch.index.codec.vectors.diskbbq.PreconditionerTests.err.log
```
