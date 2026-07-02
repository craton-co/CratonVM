# Elasticsearch REST wrong-endpoint path returns NodeSelector NPE

Status: open

Date observed: 2026-07-02

## Summary

`org.elasticsearch.client.RestClientTests.testPerformAsyncWithWrongEndpoint`
expects an invalid endpoint to fail with `IllegalArgumentException`. Under
CratonVM the async failure callback receives a `NullPointerException` because
`nodeSelector` is null.

Failure:

```text
Expected: an instance of java.lang.IllegalArgumentException
but: <java.lang.NullPointerException: Cannot invoke
"org.elasticsearch.client.NodeSelector.select(java.lang.Iterable)" because
"nodeSelector" is null> is a java.lang.NullPointerException
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found this as a single CratonVM-only failure.

```text
index=17
module=client/rest
class=org.elasticsearch.client.RestClientTests
CratonVM=FAIL, 14.944s
HotSpot=PASS, 4.205s
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 17 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-restclient-wrong-endpoint-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\client_rest.org.elasticsearch.client.RestClientTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
