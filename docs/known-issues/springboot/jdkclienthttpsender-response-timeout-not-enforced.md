# `JdkClientHttpSenderTests.sendShouldTimeoutOnSlowResponse` — `java.net.http.HttpClient` per-request response timeout not enforced

**Status: OPEN — found 2026-07-17 (hypothesis, not traced into CratonVM's HTTP client source)**

## Symptom

Module `module/spring-boot-micrometer-metrics`, class
`export.otlp.JdkClientHttpSenderTests`: 1/7 tests fails,
`sendShouldTimeoutOnSlowResponse`.

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-aff137181186.out.log`

```
=> java.lang.AssertionError:
Expecting code to raise a throwable.
   org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp.JdkClientHttpSenderTests.sendShouldTimeoutOnSlowResponse(JdkClientHttpSenderTests.java:104)
```

Test source (`JdkClientHttpSenderTests.java:100-105`):

```java
@Test
void sendShouldTimeoutOnSlowResponse() {
    JdkClientHttpSender sender = new JdkClientHttpSender(Duration.ofSeconds(5), Duration.ofMillis(10), null);
    this.mockWebServer.enqueue(new MockResponse().setResponseCode(200).setHeadersDelay(500, TimeUnit.MILLISECONDS));
    String url = this.mockWebServer.url("/test").toString();
    assertThatIOException().isThrownBy(() -> sender.get(url).send()).withMessageContaining("timed out");
}
```

`JdkClientHttpSender` is constructed with a 10ms response timeout against a
mock server that delays its response headers by 500ms — the test expects
`java.net.http.HttpClient`'s per-request `timeout(Duration)` (set via
`HttpRequest.Builder.timeout(...)`) to fire and produce an `IOException`
containing "timed out". On CratonVM, no exception is thrown at all — the
call apparently succeeds (or at least doesn't throw), meaning the response
timeout is not being enforced.

## Root cause

**Not traced into CratonVM source this session — hypothesis only.** The
other 6 tests in this class (send GET/POST/PUT/DELETE, headers, 500 status)
all pass, so basic real-socket HTTP request/response plumbing via
`java.net.http.HttpClient` works; only the timeout-specific behavior is
missing. The likely gap is in whatever native/real-bytecode path backs
`HttpRequest.Builder.timeout()`/`HttpClient`'s per-request response-timeout
enforcement — either the timeout value is silently dropped somewhere between
`HttpRequest` construction and the actual socket read loop, or the read loop
doesn't have a deadline check wired to it. This module runs with
`CRATONVM_REAL_NET_SOCKETS=1` (per the suite runner's standard config), so
the underlying `Socket`/`SocketChannel` read is real; the missing piece is
most plausibly in `java.net.http` request-level timeout wiring rather than
low-level socket I/O. No specific file/line identified — would need a
standalone repro (real `HttpClient` request against a deliberately slow
server, with `CRATONVM_DBG_*`-style tracing of the request timeout path) to
pin down.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp.JdkClientHttpSenderTests` (1 of 7 tests) |
