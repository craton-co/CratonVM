# Elasticsearch REST wrong-endpoint path returns NodeSelector NPE

Status: FIXED (2026-07-02, branch `fix/es-restclient-suite-bugs-20260702`)

## Fix

Root cause: CratonVM's `java.net.URI` single-string constructor
(`native-builtins/src/lib.rs::native_uri_init`) never validated scheme-name
syntax, so `new URI("::http:///")` silently parsed with `scheme=null`
instead of throwing `URISyntaxException("Expected scheme name", 0)` — the
real JDK `Parser.parse` scans from index 0 for the first `:`/`/`/`?`/`#`; a
`:` found at index 0 (or with a non-letter character before it) is a
malformed scheme and must fail immediately. Because CratonVM didn't throw,
`RestClient.buildUri()` never threw either, so `new InternalRequest(request)`
succeeded and execution reached `nextNodes()`/`selectNodes()` — which hit the
test's intentionally-`null` `NodeSelector` (the test only reaches
node-selection if URI validation fails to short-circuit first) — surfacing
an unrelated NPE instead of the expected `IllegalArgumentException`.

Fix: added `uri_scheme_name_fail_index`, a Rust port of the real JDK
scheme-scan algorithm (verified against HotSpot on a dozen edge cases:
`::http:///`, `:path`, `12:30`, `1abc:path`, `C:/Users/foo`,
`scheme_with_underscore:path`, `relative/path`, `mailto:...`, etc. — all
match exactly), wired into `native_uri_init` before the existing
illegal-character/empty-scheme-specific-part checks (matching real JDK's
check ordering).

Verified: `RestClientTests` PASSES against a fresh build (real ES suite
run).

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
