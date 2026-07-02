# Elasticsearch REST client builder suite timeout accounting

Status: open

Date observed: 2026-07-02

## Summary

`org.elasticsearch.client.RestClientBuilderIntegTests` fails under CratonVM with
RandomizedTesting's suite-timeout failure even though the process exits after
about 12 seconds. HotSpot passes the same class in about 7 seconds.

Failure:

```text
java.lang.Exception: Suite timeout exceeded (>= 580000 msec).
Tests run: 0, Failures: 1
```

The runner-level hang timeout was 300 seconds. This is not a runner HANG; it is
a JUnit failure emitted by the test framework.

## Full-suite result

Full suite `all[1..2701]` on 2026-07-02 with `-TimeoutSec 300` found this as a
single CratonVM-only failure. HotSpot passed the class.

```text
index=16
module=client/rest
class=org.elasticsearch.client.RestClientBuilderIntegTests
CratonVM=FAIL, 11.950s
HotSpot=PASS, 7.394s
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 16 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-restclient-builder-suite-timeout-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-full-suite-20260702\target\release\cratonvm-elasticsearch-full-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\logs\client_rest.org.elasticsearch.client.RestClientBuilderIntegTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\logs\client_rest.org.elasticsearch.client.RestClientBuilderIntegTests.out.log
```

## Notes

The mismatch points at CratonVM time accounting visible to RandomizedTesting,
not at the runner watchdog. The full-suite CratonVM result had no `HANG` rows.
