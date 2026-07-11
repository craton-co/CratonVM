# `HttpURLConnection` fixed-length streaming is deferred until response retrieval

**Status:** OPEN. **Severity:** high for applications that rely on incremental
HTTP request delivery. The earlier Acceptor/Poller scheduling diagnosis was
incorrect: the server is waiting because CratonVM's legacy
`HttpURLConnection` bridge buffers the entire request body locally.

## Root cause

`native-builtins/src/http_url_connection.rs` deliberately replaces
`setFixedLengthStreamingMode(int|long)` with no-ops and makes
`getOutputStream()` return a synthetic `ByteArrayOutputStream`. For real-JDK
`sun.net.www.protocol.http.HttpURLConnection` carriers,
`huc_real_perform()` opens the TCP connection and sends the buffered body only
when `getResponseCode()`, `getInputStream()`, or another response getter is
called.

That is incompatible with fixed-length streaming. The JDK contract requires
the request to be delivered while the caller writes it; it must not be held
until response retrieval.

## Tomcat evidence

`TestNonBlockingAPI.testNonBlockingReadIgnoreIsReady` calls:

```
postUrl(true, new DataWriter(500, 5), "http://localhost:<port>/", ...)
```

The `true` selects `setFixedLengthStreamingMode()`. A shadowed Java timeline
shows the client writing at 0, 500, 1000, 1500, and 2000 ms, while the
Tomcat Acceptor reaches native `ServerSocketChannel.accept()` immediately but
does not accept the connection until the final write/stream close triggers
response retrieval. The Poller's two 1000-ms selects merely overlap that
intentional-but-wrong local buffering interval.

The same result occurs with `--nojit`, ruling out JIT warm-up or JIT
scheduling. A minimal NIO server accepts a direct `java.net.Socket` client
within milliseconds, but fails to observe a fixed-length-streaming
`HttpURLConnection` request until the request is finalized. Therefore the
defect is not in the NIO selector, native accept loop, blocking-region GC
protocol, or Java thread scheduling.

## Required fix

Implement true streaming for real legacy HTTP carriers:

1. Record fixed-length/chunked streaming mode instead of ignoring the setters.
2. Open the TCP connection and write request headers before the first body
   write.
3. Route every output-stream write to that live connection and finish the
   request on close.
4. Make response getters read from that same connection rather than replaying
   a separately buffered request.

The existing buffered path can remain for callers that do not request a
streaming mode. Keep HTTPS, redirects, and error semantics covered when
extending the live-stream state.

## Regression oracle

Use the Tomcat single-method runner for
`testNonBlockingReadIgnoreIsReady` and `testNonBlockingRead`, plus a minimal
loopback `ServerSocketChannel` probe with a fixed-length-streaming
`HttpURLConnection`. The server must accept/read the first byte before the
client's next 500-ms write. Re-run against HotSpot as the behavioral baseline.
