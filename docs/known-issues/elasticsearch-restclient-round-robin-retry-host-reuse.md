# Elasticsearch REST round-robin retry host reuse

Status: open

Date observed: 2026-07-02

## Summary

`org.elasticsearch.client.RestClientMultipleHostsTests.testRoundRobinRetryErrors`
fails under CratonVM because the retry chain reports
`http://localhost:9200` more than once. HotSpot passes the same class.

Failure:

```text
java.lang.AssertionError:
host [http://localhost:9200] not found, most likely used multiple times
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found this as a single CratonVM-only failure.

```text
index=14
module=client/rest
class=org.elasticsearch.client.RestClientMultipleHostsTests
CratonVM=FAIL, 12.135s
HotSpot=PASS, 4.521s
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 14 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-restclient-roundrobin-host-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\client_rest.org.elasticsearch.client.RestClientMultipleHostsTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
