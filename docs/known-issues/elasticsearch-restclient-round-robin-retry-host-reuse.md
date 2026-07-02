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

The assertion removes each response host from a set and expects each host to
appear once:

```text
client\rest\src\test\java\org\elasticsearch\client\RestClientMultipleHostsTests.java:150
hostsSet.remove(response.getHost())
```

## Full-suite result

Full suite `all[1..2701]` on 2026-07-02 with `-TimeoutSec 300` found this as a
single CratonVM-only failure. HotSpot passed the class.

```text
index=14
module=client/rest
class=org.elasticsearch.client.RestClientMultipleHostsTests
CratonVM=FAIL, 9.297s
HotSpot=PASS, 4.521s
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 14 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-restclient-roundrobin-host-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-full-suite-20260702\target\release\cratonvm-elasticsearch-full-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\logs\client_rest.org.elasticsearch.client.RestClientMultipleHostsTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```

## Notes

This may be a host equality/hash-code mismatch or a retry-order bug. The result
only establishes that CratonVM reports a duplicate host where HotSpot does not.
