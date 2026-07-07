# Client `SSLSocketOutputStream.write` fails "stream is closed" mid-POST

Status: open (untriaged)

Date observed: 2026-07-07 (Azure Linux, `dev` post-merge of
`fix/netty-sslengine-underflow-20260707`; real-JDK jdk25)

## Context

Found while closing out
[`../internal/reactive-netty-https-sslengine-handshake-underflow-FIXED.md`](../internal/reactive-netty-https-sslengine-handshake-underflow-FIXED.md).
That doc's chain of fixes (SSLEngine handshake, client TrustManager
delegation, client `SSLSession` peer-chain population, and — landed alongside
this doc — the NEW-13 synthetic `SSLSocketOutputStream`/`SSLSocketInputStream`
classes missing `java.io.OutputStream`/`InputStream` as their superclass,
which caused a `ClassCastException` on the client's own output stream)
together get `ServerHttpsRequestIntegrationTests::checkUri()` all the way to
attempting the actual HTTP POST write. That write now fails:

```
FAILCAUSE ServerHttpsRequestIntegrationTests :: checkUri() ::
  org.springframework.web.client.ResourceAccessException: I/O error on POST
  request for "https://localhost:PORT/foo": SSLSocketOutputStream.write:
  stream is closed
```

("stream is closed" is the literal message from
`phases_late.rs`'s NEW-13 `SSLSocketOutputStream.write` native — see
`"SSLSocketOutputStream.write: stream is closed"` at the two guard sites
around line 40346/40361.)

## Working hypothesis

The client socket (or its underlying `s2_registry` TLS stream) is being
closed before HttpClient5 writes the POST body — plausibly:
- a connection-pooling/keep-alive path closes the socket right after the
  handshake, before the caller gets to write (race between whatever marks
  `NEW13_SOCK_CLOSED` / removes the stream from `s2_registry` and the actual
  request write), or
- HttpClient5's `SSLConnectionSocketFactory` / `ManagedHttpClientConnection`
  does an extra `getOutputStream()` or session-info probe (e.g. peer
  certificate check, ALPN query) that CratonVM's socket implementation
  mishandles as "done with this socket" and closes it, or
- the newly-populated client peer chain / trust-check path (this doc's own
  precursor fixes) closes the socket in a success path by mistake (compare
  `new13_do_create_socket`'s fail-closed `s2_tls_close` calls — make sure
  none of them fire on the SUCCESS path).

Not yet root-caused — needs a repro with a write/close call-site trace (e.g.
temporary logging in `new13_do_create_socket`, the `SSLSocketOutputStream`
natives, and wherever `NEW13_SOCK_CLOSED` gets set to 1 / `s2_tls_close` is
invoked) to see what closes the stream between connect and the POST write.

## Repro

```bash
CP=$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)
RUNNER=/data/data/spring-suite-runner-shared
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  <cratonvm> --java-home /home/victor/jdk25 -cp "$RUNNER:$CP" \
  KRun org.springframework.http.server.reactive.ServerHttpsRequestIntegrationTests
```
Add `CRATONVM_DBG_TLS_HS=1` to also see the (now successful) handshake trace
leading up to this failure.
