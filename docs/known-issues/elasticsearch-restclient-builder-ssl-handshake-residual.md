# Elasticsearch RestClientBuilderIntegTests SSL handshake residual failures

Status: open

Date observed: 2026-07-02

## Summary

Follow-on from `elasticsearch-restclient-builder-suite-timeout.md` (moved to
`docs/internal/` — that doc's suite-timeout/hang is FIXED). Once the suite
no longer hangs, `RestClientBuilderIntegTests` runs both its test methods
but both fail with narrow, specific assertion errors — a different, much
smaller bug than the original hang.

```text
JUnit version 4.13.2
.E.E
Time: 13.795
There were 2 failures:
1) testBuilderUsesDefaultSSLContext(org.elasticsearch.client.RestClientBuilderIntegTests)
java.lang.AssertionError:
Expected: an instance of javax.net.ssl.SSLHandshakeException
     but: <org.apache.http.ConnectionClosedException: Connection is closed> is a org.apache.http.ConnectionClosedException
2) testBuilderSetsThreadName(org.elasticsearch.client.RestClientBuilderIntegTests)
java.lang.AssertionError

FAILURES!!!
Tests run: 2,  Failures: 2
```

## Analysis (not yet root-caused)

`testBuilderUsesDefaultSSLContext` first connects with the WRONG (JVM
default, untrusted) `SSLContext` — the client should reject the server's
self-signed certificate during the TLS handshake and HotSpot surfaces this
as `SSLHandshakeException`. Under CratonVM the connection IS being rejected
(the test doesn't hang or succeed) but the failure reaches Apache's async
HTTP client as `ConnectionClosedException` instead — i.e. the underlying
native-tls connect (`servlet::s2_tls_connect`,
`native-builtins/src/servlet.rs`) is very likely closing the socket instead
of surfacing a `native_tls::Error` that our JDK-facing wrapper can map to
`SSLHandshakeException`, OR the async reactor is racing the close against
the handshake-failure exception and losing it. Not yet investigated further
— the fix chain in the internal doc already root-caused six distinct,
deep TLS/keystore gaps in one session and this looked like a good stopping
point for a fresh investigation.

`testBuilderSetsThreadName` fails with a bare `AssertionError` (no message)
— could be a secondary effect of the same handshake-exception-shape issue
(the test's `onFailure` callback asserts on `Thread.currentThread().getName()`
only in the SUCCESS path — a bare `AssertionError` with no message suggests
`fail()` or a JUnit `assertTrue` without a message fired, possibly the
`latch.await(10, SECONDS)` returning false, i.e. a hang/timeout on this
specific sub-test — needs isolated investigation, may be unrelated to #1).

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start <N> -Count 1 -Parallel 1 -TimeoutSec 120 `
  -RunName es-restclient-ssl-residual-repro `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir <workdir> `
  -Exe <cratonvm.exe>
```

`<N>` = the current line number of `org.elasticsearch.client.RestClientBuilderIntegTests`
in `C:\craton\CratonVM\apps\elasticsearch\cratonvm-suite\results.jit.all.tsv`
— this drifts between suite-list refreshes (observed shifting by several
positions across runs on 2026-07-02), so look it up fresh each time rather
than trusting a previously-recorded index.

## Evidence

```text
C:\craton\CratonVM-es-restclient-fixes-20260702\apps\elasticsearch-suite-runner\.suite6\results\es-sslfix-verify-20260702\all-jit\logs\client_rest.org.elasticsearch.client.RestClientBuilderIntegTests.out.log
```
