# `CloudFoundryReactiveActuatorAutoConfigurationTests.skipSslValidation`: `MockWebServer.close()` throws `AssertionError: Gave up waiting for queue to shut down`

**Status: OPEN — found 2026-07-19**

This is a **new, distinct** residual uncovered while closing
[`spring-boot-cloudfoundry-rerun-20260717-FIXED.md`](../../internal/springboot/spring-boot-cloudfoundry-rerun-20260717-FIXED.md).
That doc's two original root causes (skip-SSL-verification not honored, and a
`$Proxy` layout-probe livelock blocking `CloudFoundryReactiveActuatorAutoConfigurationTests`/
`CloudFoundryActuatorAutoConfigurationTests` before any test method ran) are
now both fixed and verified. Fixing them let
`CloudFoundryReactiveActuatorAutoConfigurationTests` actually execute all 14
of its test methods for the first time — which surfaced this separate,
previously-invisible bug in its very last test, `skipSslValidation`.

## Symptom

```
JUnit Jupiter:CloudFoundryReactiveActuatorAutoConfigurationTests:skipSslValidation()
    => java.lang.AssertionError: Gave up waiting for queue to shut down
       java.lang.AssertionError.<init>(AssertionError.java:76)
       mockwebserver3.MockWebServer.close(MockWebServer.kt:417)
       okhttp3.mockwebserver.MockWebServer.close(MockWebServer.kt:184)
       org.springframework.boot.cloudfoundry.autoconfigure.actuate.endpoint.reactive.CloudFoundryReactiveActuatorAutoConfigurationTests.skipSslValidation(CloudFoundryReactiveActuatorAutoConfigurationTests.java:328)
```

13/14 tests in the class pass; only `skipSslValidation` fails, and — critically
— it fails **inside the test's own `try (MockWebServer server = ...)`
auto-close**, i.e. *after* the test's real assertions
(`assertThat(response.getStatusCode()).isEqualTo(HttpStatusCode.valueOf(204))`,
line 325-326) already passed. The skip-SSL-validation behavior itself is
confirmed working correctly; this is purely a teardown/shutdown defect.

## Root cause (hypothesis, not confirmed at file:line precision)

`mockwebserver3.MockWebServer.close()` (bytecode-level inspection of
`mockwebserver3-5.1.0.jar`, no source jar available in this environment's
Gradle cache) does, roughly:

```kotlin
serverSocket?.closeQuietly()
for (queue in taskRunner.activeQueues()) {
  if (!queue.idleLatch().await(5, SECONDS)) {
    throw AssertionError("Gave up waiting for queue to shut down")
  }
}
taskRunnerBackend.shutdown()
```

Each accepted connection is handled by `MockWebServer$SocketHandler.handle()`,
run as ONE task on its own `TaskQueue` (`serveConnection` schedules it via
`TaskQueue.execute()`). `handle()`'s HTTP/1.1 path loops
(`while (processOneRequest(socket, source, sink)) { }`) to support
keep-alive — after serving the client's one request, it blocks again reading
the *next* request from the same (still-open) socket. Reactor Netty's
`HttpClient`/`WebClient` (used by `skipSslValidation`'s `SecurityService`,
`native-builtins/src/t27_tls.rs`'s SSLEngine bridge on the client side) does
not necessarily close its connection immediately after the response
completes — connection-pool reuse is the point of keep-alive — so this
blocking read can legitimately have no data pending when the test's `try`
block exits and calls `close()`.

The server-side blocking read for that keep-alive line goes through
`crate::servlet::s2_tls_read` → `crate::t27_tls::rustls_stream_read` →
`TlsServerStream::Rustls(StreamOwned).read()`, ultimately blocking on the raw
`TcpStream` handed to `rustls_server_wrap_existing_socket`
(`native-builtins/src/t27_tls.rs`) by
`crate::net_phase_e::take_raw_socket_stream_for_tls`. Unlike
`rustls_server_accept` (the `SSLServerSocket.accept()` path, which explicitly
sets `set_read_timeout(Some(30s))`/`set_write_timeout` on its accepted
`TcpStream`), `rustls_server_wrap_existing_socket`'s stream has **no read
timeout at all** — so once that keep-alive read starts blocking, it blocks
indefinitely (well past `close()`'s 5-second budget) unless the peer
independently sends more bytes or closes the TCP connection first.

**Not yet confirmed**: whether real HotSpot reliably avoids this exact race
(and if so, by what mechanism — e.g. a shorter default socket/Okio timeout
Java's `MockWebServer`/`Okio.source(Socket)` applies that this environment's
synthetic `SSLSocket`/`SSLSocketInputStream` doesn't inherit or honor), or
whether it's normally avoided simply because Reactor Netty in a real JVM
closes/returns the connection to its pool promptly enough that the keep-alive
read gets satisfied (a `FIN`/RST, ending `processOneRequest`'s loop) well
inside the 5-second window in practice, and CratonVM's connection lifecycle
differs enough (e.g. via the same underlying `SSLSocket` ALPN/client-mode gaps
fixed alongside this investigation, see the FIXED doc) that the equivalent
close never reaches the server side.

## Suggested next step

Give the `TcpStream` in `rustls_server_wrap_existing_socket` a read timeout
(mirroring `rustls_server_accept`'s existing convention), short enough that a
stalled keep-alive read reliably resolves (with an `IOException`, silently
caught by `MockWebServer$SocketHandler.handle()`'s own `catch (IOException)`
at `java.util.logging.Level.FINE`) before a typical test's `close()` call — or
investigate whether Reactor Netty's client-side connection close/dispose
should be reaching the server socket sooner than it currently does under
CratonVM. Not attempted this session — this doc's job is to hand off a
precisely-scoped, reproducible residual, not to guess at a fix without
evidence for which of the two explanations above is correct.

## Affected classes

| Module | Class | Notes |
|---|---|---|
| `module/spring-boot-cloudfoundry` | `org.springframework.boot.cloudfoundry.autoconfigure.actuate.endpoint.reactive.CloudFoundryReactiveActuatorAutoConfigurationTests` | 13/14 PASS; only `skipSslValidation` fails, in teardown |

Likely broader blast radius: any `mockwebserver3`/`okhttp3.mockwebserver`-based
test where the HTTP client doesn't explicitly close its connection before the
test closes the `MockWebServer` — this is very unlikely to be
CloudFoundry-specific, but no other affected class has been identified yet.

## Validation evidence

`C:\craton\CratonVM-cloudfoundry-target-20260718-019f7606\release\cratonvm-cloudfoundry-closure-r36-20260719.exe`,
run `cloudfoundry-reactive-r37-20260719`
(`apps/spring-boot-suite-runner/.suite/results/cloudfoundry-reactive-r37-20260719/all-jit/`):
14 tests, 13 passed, 1 failed (`skipSslValidation`, the `AssertionError` above).
