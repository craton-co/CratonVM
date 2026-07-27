# Elasticsearch REST round-robin retry host reuse

Status: FIXED (2026-07-02, branch `fix/es-restclient-suite-bugs-20260702`)

## Fix

Root cause: `native_collections_min`/`native_collections_max`
(`../../../../native-collections/src/lib.rs`, `Collections.min`/`Collections.max`)
compared elements via a string-decode helper (`val_to_string`, effectively
`read_string()`) instead of dispatching real `Comparable.compareTo`. For any
element that isn't a `java.lang.String` — e.g. `RestClient`'s internal
`DeadNode` (wraps `DeadHostState`, compared by `deadUntilNanos`) —
`read_string()` returns `None`/`""` for every element, so the `>`/`<`
string comparison was always false and `Collections.min`/`max` silently
returned element 0 regardless of true ordering. `RestClient.selectNodes`'s
`Collections.min(selectedDeadNodes)` dead-host revival pick therefore always
"revived" the first-registered host instead of the one with the lowest
`deadUntilNanos`, producing the "host used multiple times" failure.

Fix: replaced both functions with a shared `native_collections_extreme`
that dispatches via the existing `compare_via_compare_to` helper (real
`Comparable.compareTo`, already used by `Arrays.sort`/`Collections.sort`).
Also fixed empty-collection behavior to throw `NoSuchElementException`
(matches real JDK javadoc; previously silently returned `null`).

Verified: `RestClientMultipleHostsTests` PASSES against a fresh build,
5/5 runs across different random seeds (the test's `numNodes`/`numIters`
are randomized per JUnit seed).

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
