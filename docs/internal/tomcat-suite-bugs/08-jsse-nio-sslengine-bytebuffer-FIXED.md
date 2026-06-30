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

## Remaining (OPEN) — mTLS client-cert presentation

Full client-cert tests (`TestClientCertTls13`, `TestClientCert`) still fail with
HTTP `-1` / null body: the handshake now reaches the server's
`CertificateRequest`, but the **client never presents its certificate**. Two
gaps:

1. The production client config builders pass `client_auth = None`
   (`build_client_config(roots, …, None)` at t27_tls.rs:1397;
   `default_engine_client_config` uses `.with_no_client_auth()`), so the rustls
   client sends no cert.
2. The TLS identity is a single **process-global** `runtime_tls_identity`. In an
   in-process mTLS test both the server keystore and the client keystore call
   `install_identity_from_der`, clobbering each other — there is no
   per-`SSLContext` identity to distinguish "this engine is the client, present
   the client keystore" from "this engine is the server".

Fixing this requires per-`SSLContext`/per-engine identity tracking (associate
the `KeyManager`'s key+cert with the specific context rather than a global
slot) and wiring the client identity into `build_client_config`'s `client_auth`.
That is a distinct, larger feature than the handshake fix above.

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  org.apache.tomcat.util.net.TestSsl                 # plain HTTPS — now handshakes
# env: CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
# CWD: apps/tomcat ; wire: CRATONVM_SOCKET_CAPTURE=<prefix>
```
