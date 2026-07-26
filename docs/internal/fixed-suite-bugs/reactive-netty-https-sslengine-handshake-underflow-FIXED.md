# Reactor-Netty HTTPS server: SSLEngine handshake stalls at `BUFFER_UNDERFLOW` → `closeOutbound` [FIXED]

Status: ✅ **FIXED 2026-07-07** (branch `fix/netty-sslengine-underflow-20260707`).
The doc's documented defect — the server engine's first `do_unwrap` returning
`BUFFER_UNDERFLOW consumed=0` against a Netty-supplied buffer, then `do_wrap`
short-circuiting `CLOSED_OUTBOUND` — is resolved. With `CRATONVM_DBG_TLS_HS=1`
the handshake now runs to completion (`hs=FINISHED`) on the same
`ServerHttpsRequestIntegrationTests::checkUri()` repro:

```
do_unwrap id=3 SRC class=java/nio/DirectByteBuffer backing=direct(addr=0x...) layout=Named pos=0 lim=517 cap=2048
do_unwrap id=3 RETURN(normal) status=OK hs=NEED_WRAP consumed=517 produced=0
...
do_unwrap id=3 RETURN(normal) status=OK hs=FINISHED consumed=74 produced=0
```

## Root cause and fix

`bb_view` (`../../../native-builtins/src/t27_tls.rs`) resolved `HeapByteBuffer` fields
by name (the fix behind the earlier Tomcat-NIO SSLEngine bug), but had no
resolution path for a **`DirectByteBuffer`** — the shape Reactor-Netty's
`SslHandler` actually hands the engine (backed by native memory via an
`address` field, no heap-array `hb`/`offset`). Netty's first inbound record
therefore looked like a zero-length buffer to `do_unwrap`: `consumed=0`,
`BUFFER_UNDERFLOW`, and the subsequent `do_wrap` treated the still-`NEED_UNWRAP`
handshake state as unrecoverable and closed the outbound side.

Fix: extended `bb_view`'s by-name field resolution to recognize the
`DirectByteBuffer` shape (native `address` + `position`/`limit`/`capacity`,
no backing array) alongside the existing heap-array/`arrayOffset` shapes, so
`do_unwrap`/`do_wrap` read/write through the native buffer instead of treating
it as empty.

## Four further bugs found closing out the same test (stacked, in handshake order)

1. **`SSLEngineResult` accessor natives clobbered the REAL enum singletons.**
   Once the DirectByteBuffer fix let the handshake actually progress, the
   `HandshakeStatus`/`SSLEngineResult.Status` accessor natives were found
   overwriting the process-global REAL enum constant objects instead of
   reading them — harmless while the handshake stalled at the first call, but
   surfaced as soon as multiple wrap/unwrap round-trips happened. Fixed
   alongside a `CRATONVM_DBG_TLS_HS` trace extension.
2. **Client `SSLSocketFactory.createSocket` never consulted real Java
   `TrustManager`s.** The native-tls client-socket path
   (`phases_late.rs::new13_do_create_socket`) verified the server cert only
   against native-tls WebPKI + gathered keystore roots; a real
   `TrustManager` installed via `SSLContext.init` (e.g. HttpClient5's
   `SSLContextBuilder.loadTrustMaterial(TrustSelfSignedStrategy)` — the only
   trust source for Netty's ephemeral `SelfSignedCertificate` test server) was
   never consulted, so OpenSSL/native-tls rejected the self-signed cert before
   Java ever got a say. JSSE semantics make the TrustManager THE verifier.
   Fixed: when the factory's `SSLContext` carries attached Java
   TrustManagers, the connector is built with
   `danger_accept_invalid_certs`/`hostnames` and `checkServerTrusted` is run
   against the captured peer chain immediately after connect — fail-closed
   (no chain, or a TrustManager throw, aborts the socket with
   `SSLHandshakeException`).
3. **`SSLSession.getPeerCertificates()` threw `IllegalStateException` instead
   of `SSLPeerUnverifiedException` on an empty peer chain**, breaking Spring's
   `DefaultSslInfo.initCertificates`, which specifically catches
   `SSLPeerUnverifiedException` to mean "no client cert" when building
   `SslInfo` for a server-side session. Fixed to throw the correct exception
   type (matches the real-JDK contract, per this function's own prior doc
   comment).
4. **The client's own `SSLSession` never had its peer chain recorded.**
   `session_peer_certs_table` (the side-table `getPeerCertificates()` reads)
   was only ever populated for SSLEngine-based (server/NIO) sessions; the
   native-tls client-socket path (`new13_alloc_ssl_session`) allocated a
   session object but never inserted its already-captured peer chain into the
   table. So even after fix #3 threw the *correct* exception type, it still
   fired for the wrong reason — the client session's own chain was always
   empty. Fixed by recording the chain (already available via
   `servlet::s2_tls_peer_cert_chain_der`, the same helper fix #2 uses for the
   trust check) into the table when the client session is allocated
   (`t27_tls::record_client_peer_chain`).

## A fifth bug found while landing this fix: synthetic stream classes missing their superclass

Once the four bugs above got the handshake completing and the client past
its own peer-chain/trust checks, `checkUri()` advanced to attempting the
actual HTTP write and hit `ClassCastException: javax.net.ssl
.SSLSocketOutputStream cannot be cast to java.io.OutputStream`.
`ensure_synthetic_class` (`../../../classloading/src/class_manager.rs`) gives every
synthetic stub class a blanket `java/lang/Object` superclass; the NEW-13
client socket's synthetic `javax/net/ssl/SSLSocketOutputStream` /
`SSLSocketInputStream` classes (`phases_late.rs`) need `java.io.OutputStream`
/ `InputStream` instead so `instanceof`/`checkcast` against those real JDK
types succeeds, matching the existing `Proxy$Instance` special case in the
same function. Fixed by special-casing both class names there.

## Residual: test still red, one more (unrelated) bug found

`ServerHttpsRequestIntegrationTests::checkUri()` still fails, now on
`SSLSocketOutputStream.write: stream is closed` during the actual POST body
write — the client socket/TLS stream appears to be closed before the write
happens, a socket-lifecycle bug unrelated to TLS handshake/trust/casting.
Tracked separately in
[`../known-issues/netty-client-socket-write-after-close-nsme.md`](../known-issues/netty-client-socket-write-after-close-nsme.md).

## Verification

```
CP=$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)
RUNNER=/data/data/spring-suite-runner-shared
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_TLS_HS=1 \
  <cratonvm> --java-home /home/victor/jdk25 -cp "$RUNNER:$CP" \
  KRun org.springframework.http.server.reactive.ServerHttpsRequestIntegrationTests
```
Before: `do_unwrap ... BUFFER_UNDERFLOW consumed=0` then `CLOSED_OUTBOUND_SHORT_CIRCUIT`
(no ClientHello ever read). After: full handshake trace to `hs=FINISHED`,
failure moved to the unrelated Socket bug above.
