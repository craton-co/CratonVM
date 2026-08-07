# A client TLS handshake that fails certificate validation is reported as "interrupted", ~30 s late

## Status
**OPEN, reproduced on demand 2026-08-07.** Split out of the
`TestPgServer/TestTools/TestMemoryUnmapper/TestFileLock/TestTimer` cluster
(retired as resolved the same day) — it is the one CratonVM-side divergence
that survived those fixes. It changes no test's PASS/FAIL verdict that has
been found so far; it changes the exception type, the message, and how long
the caller waits.

## What HotSpot does, and what CratonVM does

Both VMs reject the same certificate. Only the report differs.

```
HOTSPOT 25.0.3, immediately:
  javax.net.ssl.SSLHandshakeException: (certificate_unknown) PKIX path building failed:
  sun.security.provider.certpath.SunCertPathBuilderException:
  unable to find valid certification path to requested target

CRATONVM, after ~30 s (measured 60.8 s when the handshake ran inside connect(),
i.e. two 30 s legs):
  java.io.IOException: TLS handshake failed: the handshake process was interrupted
```

`the handshake process was interrupted` is `native_tls`'s Display text for
`HandshakeError::WouldBlock` — i.e. the client-side handshake did not fail, it
TIMED OUT. `servlet::s2_tls_connect` sets a 30 s read and a 30 s write timeout
on the TCP stream before calling `connector.connect(host, tcp)`, so a handshake
that stalls for any reason is indistinguishable, to the caller, from one that
was rejected — and costs 30 s either way.

The server side of the same exchange DOES see the real reason, and logs it:

```
legacy DSA TLS server handshake: the handshake failed:
error:0A000412:SSL routines:ssl3_read_bytes:sslv3 alert bad certificate:
../ssl/record/rec_layer_s3.c:1599:SSL alert number 42
```

Alert 42 is `bad_certificate`, sent BY the client. So the client had already
made its decision before it spent 30 s reaching `WouldBlock`.

## Why it matters

Callers discriminate on the exception TYPE. `javax.net.ssl.SSLHandshakeException`
is what a `catch` block written against JSSE names; a bare `java.io.IOException`
does not match it, and CratonVM's own `phases_late/ssl_security.rs` already
takes care to raise `SSLHandshakeException` on the paths where it can (see
`ensure_layered_handshake_started`'s comment about tests that "specifically
assert on the JSSE exception type for an intentionally-rejected connection").
The `s2_tls_connect` path does not.

The 30 s is the second half: a test that expects a rejected connection to fail
fast instead waits out a socket timeout.

## Reproducer

`TlsProbe.java` — the shape of H2's `TcpServer.isRunning()`, but writing a byte
so the handshake actually runs:

```java
ServerSocket ss = org.h2.util.NetUtils.createServerSocket(9101, true);
new Thread(() -> { try (Socket s = ss.accept()) { s.getInputStream().read(); } }).start();
try (Socket c = org.h2.util.NetUtils.createLoopbackSocket(9101, true)) {
    c.getOutputStream().write(7);          // forces the handshake
}
```

```bash
cd apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
$JDK25/bin/java -cp "$CP:." TlsProbe ssl 9101          # PKIX, immediate
<cratonvm-bin> --java-home $JDK25 --nojit -c "$CP:." TlsProbe ssl 9103   # "interrupted", ~30 s
```

H2 supplies the certificate here (a DSA key baked into
`org.h2.security.CipherFactory`), which is also why the server leg takes the
OpenSSL `legacy DSA` fallback rather than rustls.

## Where to look

* `native-builtins/src/servlet.rs`, `s2_tls_connect` — the 30 s read/write
  timeouts and the `format!("TLS handshake failed: {}", e)` wrapper that
  flattens every `native_tls::HandshakeError` variant into one `IOException`.
  `HandshakeError::Failure` carries the real OpenSSL error; `WouldBlock`
  carries a `MidHandshakeTlsStream` that can be driven again.
* `native-builtins/src/phases_late/ssl_security.rs`,
  `new13_connect_and_handshake` — the caller, which maps whatever comes back to
  a plain `RuntimeError::IOException`. Its sibling paths in the same file build
  a typed `SSLHandshakeException` via `phases_early::throw_jca_exc`.

Two things worth separating when this is picked up: (1) reporting the right
exception TYPE and the underlying certificate error, which is a mapping fix,
and (2) not spending a socket timeout on a handshake the client has already
decided to abort, which needs the `WouldBlock` leg understood first — a
non-blocking read that legitimately needs another round trip must still be
retried, so the timeout cannot simply be shortened.

## Provenance
Found while fixing `TestTools.testSSL`'s `Expected: 0 actual: 1`, whose actual
cause (`SSLSocket.connect()` running the TLS handshake, contrary to JSSE) is
fixed — see `servlet::PENDING_CONNECT_SOCK_ID_BASE`. With that fixed,
`TestTools` fails at `TestTools.java:656`, the same line stock HotSpot 25 fails
at, because H2 sets `javax.net.ssl.keyStore` and never a trust store; upstream
H2 skips `testSSL()` under `config.ci` for that reason. This doc is only about
the reporting difference that remains at that shared failure point.
