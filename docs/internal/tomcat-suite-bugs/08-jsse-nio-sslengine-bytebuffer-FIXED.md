# Bug 08 — JSSE NIO SSLEngine handshake hang: ByteBuffer backing array (FIXED)

**Status:** ✅ FIXED (commit `a7b2d796`, merged to `dev` via `d02cca74`).
**Affected:** every Tomcat NIO HTTPS test (`TestSsl`, `TestClientCert*`,
`TestCustomSsl*`, `TestSSLHostConfig*`, HTTP/2-over-TLS, WebSocket-SSL — the
~17 TLS classes that HUNG). HotSpot: PASS.
**Repro:** `org.apache.tomcat.util.net.TestClientCertTls13` (and any HTTPS test)
hung after `Starting ProtocolHandler ["https-jsse-nio-..."]`.

## Symptom

The HTTPS server stood up and accepted a connection, then hung forever. Java
thread dump showed only idle pool workers (parked on AQS); the `main`, Acceptor,
and Poller threads were in native `select()`/socket-read and absent from the
Java-frame dump. Wire capture (`CRATONVM_SOCKET_CAPTURE`) was decisive:

```
r fd=… len=251 16030100f6010000f20303…   <- server reads a REAL 251-byte ClientHello
(… nothing — no write …)                 <- server never sends a ServerHello
```

The client (the test's rustls-backed `SSLSocket`) sent a genuine ClientHello;
the server read it at the socket level but its `SSLEngine` produced zero output,
so the client waited forever.

## Root cause — SSLEngine read the ByteBuffer backing array from the wrong slot

CratonVM has a real rustls-backed NIO `SSLEngine`
(`t27_tls.rs::register_sslengine_real`: `engine_begin` / `engine_wrap_pump` /
`engine_unwrap_pump`, registered on `sun/security/ssl/SSLEngineImpl`). It was
running (the older `phases_late.rs` fake-ClientHello stub is
`#[cfg(feature="synthetic-jdk")]`-gated and compiled OUT of the real-JDK CLI).

The bug was in `bb_view`, which read a `java.nio.ByteBuffer`'s fields by fixed
synthetic slot index: `[0]=array, [1]=pos, [2]=limit, [3]=cap`. But Tomcat's NIO
endpoint hands the engine **real-JDK `java.nio.HeapByteBuffer`** objects, whose
layout is `Buffer{mark, position, limit, capacity, address}` then
`ByteBuffer{hb, …}` — i.e. **slot 0 is `mark` (an int), not the backing array**.
So `bb_view` returned `arr = None`, and `bb_read_into`/`bb_write_from` moved
**zero bytes**: `unwrap` consumed nothing (never fed the ClientHello to rustls)
and `wrap` produced nothing (never emitted the ServerHello). The handshake could
not advance.

(`position`/`limit` happened to align at slots 1/2 in the real layout, masking
the issue for those — only the array slot was wrong, which is enough to break
everything.)

## Fix

`bb_view` now resolves `hb`/`position`/`limit`/`capacity` **by name** first
(mirroring `charset.rs::buf_state`), falling back to the synthetic 5-field
layout. Added `bb_set_pos` to advance `position` via both the named field and
the slot. (`native-builtins/src/t27_tls.rs`.)

After the fix the server emits a full ServerHello flight and plain-HTTPS
handshakes complete end to end — `TestSsl` progresses through
`testSimpleSsl` / `testSni` / `testKeyPass` (each stands up a connector, serves,
and moves on) instead of hanging on the first.

## Also: mTLS server-side client-CA fallback

`TestClientCertTls13` (mutual TLS) additionally needs the server to verify the
client cert. The runtime TLS identity (`install_identity_from_der`) never set
`client_ca_pem`, so `default_engine_server_config` failed `setNeedClientAuth`.
Now it falls back to the trust anchors gathered from every loaded
keystore/truststore (`extra_trust_roots`) — the same roots the JVM TrustManager
uses. The server now sends a `CertificateRequest` flight (handshake progresses
past the server's first message).

## mTLS client-cert auth — FIXED (merged to `dev`, `895d82dc`)

`TestClientCertTls13` now passes **all 6** parameterized cases. The full
per-`SSLContext` mutual-TLS path works end to end — handshake, client-cert
presentation, large request body, and role-based authorization.

- **Per-`SSLContext` identity** — keystore `engineLoad` stages its
  `(cert_pem, key_pem)` on a thread-local (real-mode native; the
  `KeyManagerFactory` natives are synthetic-gated, so the *real* KMF bytecode
  runs and a native hook there is dead); `SSLContext.init` claims it onto the
  context's object identity; `createSSLEngine` copies it to the engine
  (`EngineState.identity_override`); `engine_begin` builds that engine's rustls
  config from it. `HttpsURLConnection.setDefaultSSLSocketFactory` captures the
  client identity; the native HUC client (`http_url_connection::perform`) and
  `SSLSocketFactory.createSocket` now use the rustls client path, trusting the
  gathered test/truststore roots and presenting the client cert.
- **`do_unwrap` plaintext-corruption bug** — overflowed decrypted plaintext was
  stashed into `outbound` (the encrypted wrap buffer), so the next `wrap` put
  plaintext on the wire ("corrupt message of type InvalidContentType"). Fixed
  with a dedicated `EngineState.plaintext_pending`. *(General TLS fix — affects
  any large HTTPS response, not just mTLS.)*
- **Unclean-close tolerance** — the HUC client now treats a server TCP close
  without `close_notify` (rustls `UnexpectedEof`) as end-of-stream.
- **Real-mode `SSLSession.getId()` and `getPeerCertificates()`** — the latter
  returns real `X509CertImpl` mirrors of the captured rustls peer (client) cert
  chain.

The two blockers that surfaced after the handshake worked were both fixed:

1. **Authorization 401 — client cert not requested.** Tomcat configures
   client-cert auth via `SSLParameters.setNeed/WantClientAuth` +
   `engine.setSSLParameters` (NOT the engine's own setters). The engine's
   `setSSLParameters` ignored those booleans, so the server never sent a
   `CertificateRequest`, `conn.peer_certificates()` was empty, and the realm
   denied access. Fixed: `setSSLParameters` reads `need/wantClientAuth` and
   `engine_begin` requests the cert for NEED *or* WANT (optional →
   `WebPkiClientVerifier::allow_unauthenticated`). Also: the client identity is
   captured in `SSLContext.getSocketFactory()` (a client-only call) because a
   native override of the concrete `setDefaultSSLSocketFactory` body never fires.

2. **Large-POST 400 (`SocketTimeoutException`) — over-eager unwrap.** The unwrap
   decrypted whole rustls buffers, producing more plaintext than the caller's
   dst and stashing the excess in our own buffer — invisible to Tomcat, which
   then blocked on the socket for body bytes that had already arrived. Rewrote
   `do_unwrap` to be **record-oriented**: feed rustls ONE complete TLS record at
   a time (looping `read_tls` until the whole record is fed — a single `read_tls`
   only takes a partial record), only while the dst has room, leaving excess
   records in the caller's `netInBuffer`; serve overflow plaintext first and
   return `BUFFER_OVERFLOW` (post-handshake) when the dst is full so the caller
   drains and retries. This is the correct SSLEngine-over-rustls bridge and
   benefits all large request bodies.

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  org.apache.tomcat.util.net.TestClientCertTls13     # mTLS — now OK (6 tests)
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  org.apache.tomcat.util.net.TestSsl                 # plain HTTPS — handshakes + serves
# env: CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
# CWD: apps/tomcat ; wire: CRATONVM_SOCKET_CAPTURE=<prefix>
```
