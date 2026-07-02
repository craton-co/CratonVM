# Elasticsearch REST RequestOptions header list exception message mismatch

Status: open

Date observed: 2026-07-02

## Summary

`org.elasticsearch.client.RequestOptionsTests.testAddHeader` fails under
CratonVM because mutating the immutable header list throws
`UnsupportedOperationException` with an empty message. HotSpot throws the same
exception with a null message, which is what the test expects.

Failure:

```text
java.lang.AssertionError: expected null, but was:<>
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found this as a single CratonVM-only failure.

```text
index=12
module=client/rest
class=org.elasticsearch.client.RequestOptionsTests
CratonVM=FAIL, 8.479s
HotSpot=PASS, 3.217s
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 12 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-requestoptions-header-message-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\client_rest.org.elasticsearch.client.RequestOptionsTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
