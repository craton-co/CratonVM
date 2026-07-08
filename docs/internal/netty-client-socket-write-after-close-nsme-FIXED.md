# Client `SSLSocketOutputStream.write` fails "stream is closed" mid-POST [FIXED]

Status: ✅ **FIXED 2026-07-08** (branch `fix/netty-socket-write-after-close-20260708`).

Date observed: 2026-07-07 (Azure Linux, `dev` post-merge of
`fix/netty-sslengine-underflow-20260707`; real-JDK jdk25)

## Context

Found while closing out
[`reactive-netty-https-sslengine-handshake-underflow-FIXED.md`](reactive-netty-https-sslengine-handshake-underflow-FIXED.md).
That doc's chain of fixes (SSLEngine handshake, client TrustManager
delegation, client `SSLSession` peer-chain population, and the NEW-13
synthetic `SSLSocketOutputStream`/`SSLSocketInputStream` classes missing
`java.io.OutputStream`/`InputStream` as their superclass) together got
`ServerHttpsRequestIntegrationTests::checkUri()` all the way to attempting
the actual HTTP POST write. That write failed:

```
FAILCAUSE ServerHttpsRequestIntegrationTests :: checkUri() ::
  org.springframework.web.client.ResourceAccessException: I/O error on POST
  request for "https://localhost:PORT/foo": SSLSocketOutputStream.write:
  stream is closed
```

`checkUri()` builds its `RestClient` from
`HttpComponentsClientHttpRequestFactory` wrapping Apache HttpClient5's
`SSLConnectionSocketFactory`, backed by an `SSLContextBuilder` +
`TrustSelfSignedStrategy` (needed to accept `ReactorHttpsServer`'s
ephemeral self-signed test certificate). Getting this single test to a
clean, consistent pass required **eight** separate fixes — the working
hypothesis at each step was invalidated by the next `CRATONVM_DBG_TLS_SOCK`
trace or, in the final case, by decompiling Apache HttpClient5's own
bytecode.

## Root causes and fixes (in the order they were found)

### 1. `net_phase_e`-side-table read fallback for `SSLSocket` stream/lifecycle natives

`SSLSocketFactory.createSocket(String,int)` is registered in **both**
`phases_late.rs` (`new13_do_create_socket`, raw-field based — added for
Java `TrustManager` delegation) and `net_phase_e.rs` (side-table based,
registered later in `register_essential_natives` — wins the same-key
last-writer race for that overload specifically). But
`getInputStream`/`getOutputStream`/`close`/`isClosed`/`isConnected` are
registered on the concrete `javax/net/ssl/SSLSocket` class only by
`phases_late.rs`; being a more specific match than `net_phase_e`'s
registrations on the `java/net/Socket` superclass, they run regardless of
which factory built the object. Added `new13_resolve_tls_id()`: check the
raw field first, fall back to `net_phase_e::sock_stream_id_for_upcall()`
(the accessor `http_url_connection.rs` already used for the same side
table).

### 2. Missing `begin_blocking_region`/`end_blocking_region` around the TLS connect

`new13_do_create_socket` called the blocking `s2_tls_connect` (real TCP
connect + full TLS handshake) without announcing a blocking region, unlike
`net_phase_e.rs`'s own client `createSocket`, which already does (its
"T19.H1" comment: "without announcing our own blocking region here, a
concurrent stop-the-world GC would wait forever for this thread to reach a
safepoint it can't reach until the network call returns"). Fixed to match
the established pattern.

### 3. `java/net/Socket.getLocalAddress()` had no reachable native registration

Real bytecode ran instead, reading a plain `java.lang.String` (this file's
`SOCK_HOST` slot) where real `Socket` internals expect a structured
`InetAddress`/holder, throwing `NoSuchMethodError:
java/lang/String.getOption(I)Ljava/lang/Object;`. Registered a native
returning loopback (this client-side socket never does an explicit local
bind, so byte-perfect topology isn't available; a non-crashing, non-null
address is what callers need).

### 4. Side-table migration for `new13_do_create_socket`'s connection state (the original bug)

The actual root cause of the doc's namesake symptom. `alloc_concurrent_synthetic`
sizes a "synthetic" object using the **real** loaded class's own field
count/layout when the class is loadable (see its own doc comment) —
`javax/net/ssl/SSLSocket` is a real, loaded JDK class, so field index 2 on
our synthetic instance lands wherever the real class hierarchy's actual
field #2 is (reference-typed), not a slot we control. Confirmed via a
same-native-call immediate readback (no Java code, no GC in between):
`ctx.set_field(sock, 2, Value::Int(tls_id))` followed instantly by
`ctx.get_field(sock, 2)` already read back `Object(None)` — the GC/field-layout
guard silently drops a mismatched-type write rather than erroring. This is
exactly the collision `net_phase_e.rs`'s `SockSide` table was introduced to
avoid for plain `java.net.Socket`/`ServerSocket`/`DatagramSocket`; this
NEW-13 `SSLSocketFactory` implementation never got migrated. Fixed:
`net_phase_e::sock_set_for_create()` records the connect result in the
same side table `net_phase_e`'s own `createSocket(String,int)` already
uses for this exact class, so fix #1's fallback now actually finds a valid
entry.

### 5. `setEnabledCipherSuites` reconnect skipped for a non-restriction

Apache HttpClient5 calls `setEnabledCipherSuites(socket.getSupportedCipherSuites())`
unconditionally as part of its normal connection setup — passing back the
exact same 9-suite list our own `getSupportedCipherSuites()` returns, not a
genuine restriction. `net_phase_e.rs`'s `setEnabledCipherSuites` reconnects
(tears down and rebuilds the connection via a separate, Java-TrustManager-unaware
rustls path) whenever any requested cipher is rustls-mappable. This
reconnect was **previously dormant** for `phases_late.rs`-created sockets
(short-circuited by `side.port <= 0`, since the side table was never
populated for them) until fix #4 started populating it — inadvertently
activating this path for the first time and exposing the latent trust gap:
reconnecting a socket that `new13_do_create_socket` had already connected
successfully via Java `TrustManager` delegation blew that away and failed
with `invalid peer certificate: UnknownIssuer`. Fixed: don't reconnect when
the requested cipher set covers the socket's entire supported set — only
reconnect for a genuine, narrower restriction (what this path exists for
in the first place, e.g. Tomcat's `TesterSupport.ClientSSLSocketFactory`).

### 6. `SSLSession.getLocalPrincipal`/`getPeerPrincipal`/`getLocalCertificates` never registered

None of these three had a native registration anywhere in the crate.
`SSLSession` is a real JDK interface, so calling an unregistered method on
a synthetic object impersonating it throws `AbstractMethodError`. Fixed:
`getLocalPrincipal`/`getLocalCertificates` return `null` (correct per
contract — this path never presents a local/client certificate);
`getPeerPrincipal` mirrors the existing `getPeerCertificates` implementation,
wrapping the leaf cert's subject DN in the same synthetic `X500Principal`
shape used elsewhere in the crate.

### 7. `setSoTimeout`/`getSoTimeout` never checked the TLS stream table

Both natives only checked `s2_registry().streams` (plain TCP, `net_phase_e.rs`'s
own `java.net.Socket` implementation). A TLS socket's stream id lives in a
**different** table, `tls_streams` — so both natives silently no-op'd for
every TLS socket (an existing doc comment on `getSoTimeout` already noted
Apache HttpClient5's `DefaultManagedHttpClientConnection.bind()` calls both
unconditionally on every connection). Fixed: fall back to `tls_streams`,
reaching the real underlying `TcpStream`'s read timeout via
`native_tls::TlsStream::get_ref()`.

### 8. `isInputShutdown`/`isOutputShutdown` never registered (the final, flaky residual)

After fixes 1–7 the test passed roughly 65–80% of the time, failing the
rest with a generic `org.apache.hc.core5.http.ConnectionClosedException:
Connection is closed` and **no** CratonVM native or trace event anywhere
near the failure. `KRUN_STACK=1` surfaced the real throw site; decompiling
`httpcore5-5.4.2.jar`'s `DefaultBHttpClientConnection$1` (the anonymous
`OutputStream` wrapping the raw socket stream) showed every
`write()`/`write(byte[])`/`write(byte[],int,int)` calls `checkTLS(sslSocket)`
first:

```java
void checkTLS(SSLSocket s) throws IOException {
    if (s.isInputShutdown()) throw new ConnectionClosedException();
}
```

`isInputShutdown()`/`isOutputShutdown()` had no reachable native
registration in real-JDK mode — the only registration lived in
`register_p72_server_socket`, reachable solely through the
`synthetic-jdk`-gated `register_synthetic_overrides` umbrella (the same
dead-code shape already fixed once elsewhere in this codebase — see
`lib.rs`'s "FIX (httpserver-pkcs12-20260706)" comment). Real
`java.net.Socket` bytecode ran instead, reading our synthetic object's
fields as if they were the real class's `impl`/`shutIn` internals —
undefined, and non-deterministically truthy roughly one run in three,
exactly matching this bug's flaky signature. Fixed: registered real,
side-table-backed `isInputShutdown`/`isOutputShutdown` (new `SockSide`
fields `input_shutdown`/`output_shutdown`, set only by an actual
`shutdownInput()`/`shutdownOutput()` call). `shutdownInput`/`shutdownOutput`
also gained the same `tls_streams` fallback as fix #7.

## Verification

```bash
CP=$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)
RUNNER=/data/data/spring-suite-runner-shared
<cratonvm> --java-home /home/victor/jdk25 -cp "$RUNNER:$CP" \
  KRun org.springframework.http.server.reactive.ServerHttpsRequestIntegrationTests
```

- **Before fix #4** (the original bug): 100% failure, `SSLSocketOutputStream.write: stream is closed`.
- **After #4, before #5/#6**: progressed to a deterministic `invalid peer certificate: UnknownIssuer`, then `AbstractMethodError` on `getLocalPrincipal`.
- **After #5–#7, before #8**: ~65–80% pass rate, remainder `ConnectionClosedException: Connection is closed` (no native/trace event — root-caused only via `KRUN_STACK=1` + decompiling HttpClient5's own bytecode).
- **After #8**: **20/20** consecutive runs pass.
- Regression: `cargo test -p cratonvm-native-builtins --lib` — 2811 passed, 12 pre-existing failures (`lang_class`/`unsafe_jdk25`, confirmed identical on a clean `dev` checkout, unrelated to this branch). `HttpComponentsClientHttpRequestFactoryTests` (18/18) and `InterceptingStreamingHttpComponentsTests` (6/6) — both green.
