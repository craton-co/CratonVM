# Elasticsearch RandomizedTesting suite timeout accounting

Status: open

Date observed: 2026-07-02

## Summary

Some Elasticsearch tests fail under CratonVM with RandomizedTesting's suite
timeout even though the runner process exits quickly. This is a JUnit failure,
not the runner watchdog; the runner hang timeout was 300 seconds.

Representative failure:

```text
java.lang.Exception: Suite timeout exceeded (>= 580000 msec).
Tests run: 0, Failures: 1
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- `org.elasticsearch.client.RestClientBuilderIntegTests` is CratonVM-only:
  HotSpot passed it.
- `org.elasticsearch.index.codec.vectors.es816.BinaryQuantizationTests` also
  reports suite-timeout accounting, but HotSpot failed that class too, so it is
  not counted as CratonVM-only.

Representative Craton-only row:

```text
index=15
module=client/rest
class=org.elasticsearch.client.RestClientBuilderIntegTests
CratonVM=FAIL, 12.457s
HotSpot=PASS, 7.394s
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 15 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-randomized-suite-timeout-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\client_rest.org.elasticsearch.client.RestClientBuilderIntegTests.out.log
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.vectors.es816.BinaryQuantizationTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
