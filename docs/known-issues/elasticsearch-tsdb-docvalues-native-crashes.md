# Elasticsearch TSDB doc-values native crashes

Status: open

Date observed: 2026-07-02

## Summary

Two TSDB doc-values codec tests crash the CratonVM process. HotSpot also fails
these classes in the baseline, but CratonVM terminates natively with an access
violation, so this is tracked as a separate VM crash.

Representative fatal output:

```text
# A fatal error has been detected by the CratonVM Runtime Environment:
# EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005)
# Faulting access: read at address 0x0000002200000000
# thread: "Thread-2"
```

The stderr immediately before the crash also reports:

```text
Missing native method in real-JDK mode method=java/io/FileCleanable.cleanupClose0(IJ)V
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- 2 CratonVM crashes in this family.
- Both overlap HotSpot baseline failures, but HotSpot does not crash the VM.

Representative row:

```text
index=1299
module=server
class=org.elasticsearch.index.codec.tsdb.es819.ES819TSDBDocValuesFormatTests
CratonVM=CRASH, 56.100s
HotSpot=FAIL, 61.469s
```

The other class is:

```text
org.elasticsearch.index.codec.tsdb.es95.ES95TSDBDocValuesFormatTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1299 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-tsdb-docvalues-native-crash-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.tsdb.es819.ES819TSDBDocValuesFormatTests.err.log
Windows Application log, Application Error source, 2026-07-02 around 15:16 and 15:17 local time, exception code 0xc0000005
```

## No-JIT partial evidence

Run `es-nojit-full-20260702` was stopped by request after 1366 recorded
classes. In no-JIT, the same TSDB doc-values classes did not crash before the
runner timeout; they hung and were killed at 300 seconds:

```text
index=1347 org.elasticsearch.index.codec.tsdb.es819.ES819TSDBDocValuesFormatTests HANG, 300.081s
index=1356 org.elasticsearch.index.codec.tsdb.es95.ES95TSDBDocValuesFormatTests HANG, 300.129s
```

This suggests the JIT-on crash may be a compiled-code symptom of a path that
can also deadlock or spin in interpreter mode.

Evidence:

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
```
