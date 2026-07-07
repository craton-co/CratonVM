# Reactor-Netty HTTPS server: SSLEngine handshake stalls at `BUFFER_UNDERFLOW` → `closeOutbound`

**Status:** 🔴 OPEN — isolated 2026-07-07 as the FINAL (5th) blocker behind
`ServerHttpsRequestIntegrationTests::checkUri()`, after the four crypto/cert
bugs in [`../internal/http-server-sslengine-identity-singleton-clobber-FIXED.md`](../internal/http-server-sslengine-identity-singleton-clobber-FIXED.md)
were fixed (the server rustls `ServerConfig` now builds successfully, so the
failure finally advances into TLS record processing).
**Area:** VM — real-JDK `sun.security.ssl.SSLEngineImpl` unwrap/wrap ByteBuffer
data-flow (`native-builtins/src/t27_tls.rs`), driven by **Reactor-Netty**'s
`SslHandler` (distinct from the Tomcat-NIO path fixed in
`docs/internal/tomcat-suite-bugs/08-jsse-nio-sslengine-bytebuffer-FIXED.md`).
**Severity:** Medium — blocks reactive (Netty) HTTPS server tests once their
cert/key setup succeeds.

## Symptom

With `CRATONVM_DBG_TLS_HS=1`, the server engine's very first `do_unwrap`
returns `status=BUFFER_UNDERFLOW hs=NEED_UNWRAP consumed=0 produced=0`, and the
next `do_wrap` short-circuits `CLOSED_OUTBOUND` — i.e. the server consumed no
inbound bytes (never saw a readable ClientHello through Netty's buffers), then
Netty closed the connection. The client (Apache HttpClient5) sees
`TLS handshake failed: unexpected EOF`.

```
[dbg-tls-hs] install_identity_from_der ... key_len=1217   # server identity OK
[dbg-tls-hs] install_identity_from_der ... key_len=1219   # (2nd, now also valid)
[dbg-tls-hs] do_unwrap id=3 RETURN(normal) status=BUFFER_UNDERFLOW hs=NEED_UNWRAP consumed=0 produced=0
[dbg-tls-hs] do_wrap   id=3 CLOSED_OUTBOUND_SHORT_CIRCUIT
```
No `ServerConfig ... failed` / no `BadEncoding` — the config is valid; the
data-flow is the problem.

## Why this is distinct from the (fixed) Tomcat-NIO SSLEngine bug

The Tomcat NIO fix (`bb_view` resolving `HeapByteBuffer` fields by name +
record-oriented `do_unwrap`, merged as `a7b2d796`/`895d82dc`) made plain-HTTPS
Tomcat handshakes complete. Reactor-Netty drives the SSLEngine through its own
`ByteBuf`/`ByteBuffer` wrappers and a different unwrap/wrap cadence (it may hand
the engine a direct buffer, or a composite/sliced view whose backing-array slot
differs again), so the server's first `unwrap` reads zero inbound bytes on the
Netty path even though the Tomcat path works. The likely culprit is the same
`bb_view` family — a Netty buffer shape not covered by the name/slot resolution
— or an `unwrap` that reports `BUFFER_UNDERFLOW` without having consumed the
record Netty actually delivered.

## Repro (Azure spring-web suite)

```bash
CP=$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)
RUNNER=/data/data/spring-suite-runner-shared
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_TLS_HS=1 \
  <cratonvm> --java-home /home/victor/jdk25 -cp "$RUNNER:$CP" \
  KRun org.springframework.http.server.reactive.ServerHttpsRequestIntegrationTests
```
The precursor crypto/cert bugs are all fixed on current `dev`
(`fix/tls-identity-singleton-clobber-20260707`): the same test now reaches the
`BUFFER_UNDERFLOW`/`closeOutbound` above instead of failing at
`failed to parse private key` / `BadEncoding`.

## Next step

Wire `CRATONVM_SOCKET_CAPTURE=<prefix>` (the decisive method from the Tomcat
fix) to confirm whether the server actually reads the ClientHello bytes off the
socket, then trace `bb_view` / `do_unwrap` for the concrete Netty buffer class
it is handed (log the buffer's class + resolved backing array + position/limit
when `consumed=0`). Fold the fix into the same `bb_view`-by-name mechanism if it
is another buffer-shape gap. See [[reference_jsse_nio_sslengine_bytebuffer_hang]]
and `reference_tomcat_jsse_tls_chain`.
