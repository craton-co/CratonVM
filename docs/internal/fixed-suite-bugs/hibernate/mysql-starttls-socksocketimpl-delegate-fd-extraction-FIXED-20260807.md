# MySQL Connector/J STARTTLS upgrade fails — `SocksSocketImpl` delegate wraps the real `FileDescriptor` — FIXED

**Status:** FIXED 2026-08-07. Found while setting up a real-MySQL run of the
Hibernate suite. Confirmed against real HotSpot the identical JDBC
connection succeeds without ever attempting a TLS handshake at all for a
`sslMode=DISABLED`-style config, while CratonVM always attempted (and
failed) one — the real bug turned out to be upstream of `sslMode` entirely:
CratonVM's TLS-upgrade socket extraction silently falls back to dialing a
**brand-new** connection instead of upgrading the existing one, which is
invisible for most protocols but fatal for MySQL's wire protocol.

## Symptom

Any MySQL JDBC connection through CratonVM fails during the TLS upgrade
step with:

```
javax.net.ssl.SSLHandshakeException: handshake process: received corrupt message of type InvalidContentType
	at com.mysql.cj.protocol.ExportControlled.performTlsHandshake(ExportControlled.java:208)
	at com.mysql.cj.protocol.StandardSocketFactory.performTlsHandshake(StandardSocketFactory.java:183)
	at com.mysql.cj.protocol.a.NativeSocketConnection.performTlsHandshake(NativeSocketConnection.java:91)
	at com.mysql.cj.protocol.a.NativeProtocol.negotiateSSLConnection(NativeProtocol.java:356)
	at com.mysql.cj.protocol.a.NativeAuthenticationProvider.connect(NativeAuthenticationProvider.java:199)
```

No connection property combination avoided it (`sslMode=DISABLED`,
`useSSL=false`, `requireSSL=false`, `verifyServerCertificate=false`, tried
individually and combined, with both `localhost` and `127.0.0.1`) — every
attempt hit the identical error. Confirmed on real HotSpot the identical
JDBC URL and `hibernate.properties` config never invoke
`negotiateSSLConnection`/`performTlsHandshake` at all; the connection
succeeds cleanly with zero SSL negotiation. This ruled out a connection-
property mistake and pointed at a genuine CratonVM/HotSpot behavioral
divergence for the same Connector/J bytecode.

## Root cause

MySQL's wire protocol is a mid-stream TLS upgrade (STARTTLS-style): the
client and server exchange a plaintext handshake packet first (the
`SSLRequest` packet), and *then* the connection is upgraded to TLS on the
*same already-connected socket* — unlike a protocol that speaks TLS as the
very first bytes on the wire.

CratonVM's `take_raw_socket_stream_for_tls`
(`native-builtins/src/net_phase_e.rs`) implements this upgrade by pulling
the raw OS file descriptor out of the wrapped `java.net.Socket`'s `impl`
field and handing that descriptor to rustls directly, so the *same*
connection continues under TLS. The extraction only ever looked at
`impl.fd` directly.

Since JDK 13, `Socket.createImpl()` unconditionally wraps the real
`SocketImpl` in `java.net.SocksSocketImpl` (a `DelegatingSocketImpl`) so
that a SOCKS proxy configured *after* construction is still honored — this
applies to *every* client-side `new Socket()` + `.connect(...)`, including
the plain, non-SOCKS connections `com.mysql.cj.protocol.StandardSocketFactory`
creates, not just ones that actually use a proxy. Confirmed via reflection
on both VMs: `impl.getClass()` is `java.net.SocksSocketImpl` and `impl.fd`
is `null` on both HotSpot and CratonVM — the live `FileDescriptor` sits one
level deeper, at `impl.delegate.fd`.

Because the old extraction only checked `impl.fd`, it always found nothing
for this socket shape and silently fell back to
`PendingLayeredStream::DialFresh` — opening a **brand-new** TCP connection
to the same host:port instead of upgrading the existing one. This fallback
is invisible for a protocol that speaks TLS as its first bytes (a fresh
dial looks identical to the original connection from the TLS handshake's
point of view). But MySQL has already exchanged plaintext protocol bytes on
the original socket before requesting the upgrade — the fresh dial instead
hands rustls the server's plaintext initial-handshake response as if it
were the first TLS record, which fails immediately with exactly the
observed error (rustls's `Display` for `InvalidMessage::InvalidContentType`
— produced when the first bytes read aren't a valid TLS record).

## Fix

`native-builtins/src/net_phase_e.rs`, `take_raw_socket_stream_for_tls`:
walk up to 4 levels of `impl.delegate` before giving up, so a
`SocksSocketImpl`-wrapped (or any further-nested `DelegatingSocketImpl`)
socket's real `FileDescriptor` is found instead of triggering the
dial-fresh fallback:

```rust
let mut descriptor = None;
for _ in 0..4 {
    match ctx.get_field_by_name(implementation, "fd") {
        Value::Object(Some(d)) => { descriptor = Some(d); break; }
        _ => match ctx.get_field_by_name(implementation, "delegate") {
            Value::Object(Some(next)) => implementation = next,
            _ => break,
        },
    }
}
```

The bound of 4 is defensive (a real SOCKS chain or a future JDK adding
another wrapper layer), not load-bearing for this specific case (one level
of unwrapping is what's actually needed here).

## Verification

- Repro, default `sslMode` (no workaround properties):
  `org.hibernate.orm.test.jpa.EntityManagerTest` — `found=16 ok=16 failed=0`
  against a real MySQL 9.7 server, real TLS upgrade completing successfully.
- Second, independent class: `org.hibernate.orm.test.jpa.criteria.InPredicateTest`
  — `found=1 ok=1 failed=0`.
- `cargo test --release -p cratonvm-native-builtins --lib -- tls ssl`:
  **258 passed, 0 failed, 1 ignored** — no regression to CratonVM's existing
  TLS/SSLEngine support (Tomcat mTLS, X509TrustManager, ALPN, etc.).

## Related

- `postgres-scram-sha256-pbkdf2-hmacsha384-missing-20260807-FIXED.md`
  — a separate crypto/auth gap found in the same real-database investigation
  session (Postgres SCRAM auth, not TLS-upgrade socket handling — different
  mechanism, same investigation). Closed 2026-08-12.
