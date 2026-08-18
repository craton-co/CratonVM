# A client TLS handshake that fails certificate validation is reported as "interrupted", ~30 s late — FIXED 2026-08-10

## Status
**FIXED 2026-08-10.** Both halves the original page asked to separate are
closed, and the second one turned out not to be what the page thought it was.

Was `docs/known-issues/h2/bug-cratonvm-tls-client-handshake-failure-reported-as-interrupted-20260807.md`
(OPEN 2026-08-07 → 2026-08-10).

## What the page reported

```
HOTSPOT 25.0.3, immediately:
  javax.net.ssl.SSLHandshakeException: (certificate_unknown) PKIX path building failed: …

CRATONVM, after ~30 s:
  java.io.IOException: TLS handshake failed: the handshake process was interrupted
```

`the handshake process was interrupted` is `native_tls`'s Display text for
`HandshakeError::WouldBlock` — the client-side handshake did not fail, it TIMED
OUT against the 30 s read timeout `servlet::s2_tls_connect` sets. The page split
that into (1) a reporting/mapping fix and (2) "not spending a socket timeout on
a handshake the client has already decided to abort", and said (2) "needs the
`WouldBlock` leg understood first".

## (2) was not a client-side timeout policy question at all

The page's framing — the client had already decided, and then burned 30 s —
implies the client was waiting on itself. It was not. Three arms of the same
reproducer, H2's own `NetUtils` on both ends, separate `--nojit` runs:

| arms | client outcome | elapsed |
| --- | --- | --- |
| HotSpot server + HotSpot client | `SSLHandshakeException` (PKIX) | 265 ms |
| HotSpot server + **CratonVM client** | `IOException: … Connection reset by peer` | **167 ms** |
| **CratonVM server** + HotSpot client | `SSLHandshakeException` (handshake_failure) | **281 ms** |
| CratonVM server + CratonVM client (one process) | `IOException: … interrupted` | **30 272 ms** |

Each CratonVM leg on its own answers in a few hundred milliseconds. The 30 s
appears only when BOTH ends are CratonVM — which is not a property of either
leg, so it cannot be a client timeout policy.

Timestamped, in one process:

```
[67ms]    server socket created
[568ms]   client connecting
[604ms]   SERVER: legacy DSA TLS server handshake: the handshake failed: unexpected EOF
[30980ms] CLIENT: TLS handshake failed: the handshake process was interrupted
```

The server's handshake ends 36 ms after the client connects, reading **EOF** —
it got a connection nobody spoke on. Then the client waits out its full read
timeout on a connection nobody is accepting.

## Root cause: `SSLSocket.connect()` opened a connection and threw it away

JSSE splits connect from handshake, and CratonVM models that with the
`PENDING_CONNECT_SOCK_ID_BASE` id range: `connect()` parks the endpoint and the
handshake runs at the first read/write. But the connect step was a *probe* —
`new13_tcp_reachability_probe` did `TcpStream::connect(...).map(|_| ())` and
dropped the stream — and the deferred handshake later called `s2_tls_connect`,
which **dialled a second time**.

So one `SSLSocket` produced TWO TCP connections. A server that accepts one
connection per client (H2's `TcpServer`, and the fixture) accepts the FIRST —
the dead probe connection, whose handshake reads EOF — and is no longer in
`accept()` when the real one arrives. The real connection sits unaccepted in the
listen backlog until the client's 30 s `SO_RCVTIMEO` fires, at which point
OpenSSL reports `WANT_READ` and `native_tls` returns `HandshakeError::WouldBlock`
— "the handshake process was interrupted".

This is invisible whenever the peer is a real server that keeps accepting (a
second connection just gets served and abandoned), which is why every
cross-VM arm above looks healthy and only the one-process arm shows it.

## The fix

* `new13_tcp_reachability_probe` now RETURNS the connected `std::net::TcpStream`
  instead of dropping it, and `new13_ssl_socket_connect` parks it on
  `PendingConnectSocket::tcp`.
* `servlet::s2_tls_connect_on` / `s2_legacy_dsa_tls_connect_on` run the
  handshake over an already-connected stream;
  `new13_connect_and_handshake_on` hands the parked one through from
  `ensure_layered_handshake_started`. The no-stream forms still dial, so the
  immediate-connect `createSocket(host, port)` overloads are unchanged.
* `close()` on a socket that connected and was never used still drops the
  parked entry (`drop_pending_connect_socket_if_any`), which now also closes
  the socket — so H2 `TcpServer.isRunning()`'s connect-and-close still costs
  exactly one connection and no handshake, which is what the deferral exists
  for in the first place.

## (1) the exception TYPE, closed too

`s2_tls_connect` flattened every failure into one `io::Error`, and
`new13_connect_and_handshake` mapped that to `RuntimeError::IOException`. A JSSE
caller catches `javax.net.ssl.SSLHandshakeException`, which a bare
`java.io.IOException` does not match.

`servlet::TlsConnectFailure` now separates `Tcp(io::Error)` from
`Handshake(String)`: an unreachable peer stays an `IOException`, a rejected
handshake raises `javax/net/ssl/SSLHandshakeException` via
`phases_early::throw_jca_exc`, carrying the backend's own text.

## After

```
[604ms] SERVER: legacy DSA TLS server handshake: the handshake failed:
        error:0A000412:…:sslv3 alert bad certificate:… SSL alert number 42
[605ms] CLIENT: javax.net.ssl.SSLHandshakeException: TLS handshake failed:
        error:0A000086:…:tls_post_process_server_certificate:certificate verify
        failed:… (EE certificate key too weak)
```

30 272 ms → **605 ms**, `IOException` → `SSLHandshakeException`, and the message
names the certificate error. The server now sees alert 42 (`bad_certificate`)
from the client — the same exchange the original page quoted from its
server-side log, which it could only ever observe because the client had
managed one good handshake attempt before the pathological one.

HotSpot's text is PKIX-flavoured and CratonVM's is OpenSSL-flavoured; the
divergence that mattered (type, and 30 s) is gone. Both VMs reject the same
certificate at the same point.

> **AMENDED 2026-08-16, twice — read this before citing the paragraph above.**
>
> 1. **There was a SECOND cause of the same 30 s `WouldBlock`, and this page's
>    fixture could not see it.** `TlsBoth` makes exactly ONE client connection.
>    `SSLServerSocket.accept()` ran the handshake inline and threw its failure
>    out of `accept()`, so the FIRST failed handshake killed the server's accept
>    loop — and H2 supplies one before any real client, because
>    `TcpServer.isRunning()` connects and closes without I/O. With a probe that
>    connects three times before the real client (`TlsProbe2`), pristine `dev`
>    still gave `30 162 ms` + "the handshake process was interrupted" as late as
>    2026-08-16. Fixed by deferring the failure to the accepted socket, as JSSE
>    does; the retired
>    `bug-h2-suite-fail-cluster-pgserver-tools-memoryunmapper-filelock-timer-20260807`
>    page carries the measurements. `TestTools`: 38 s → 4.9 s.
> 2. **"Both VMs reject the same certificate" is only true as H2 configures
>    it.** With H2's keystore ALSO installed as the trust store, HotSpot
>    completes the handshake (197 ms) and CratonVM still refuses it
>    ("EE certificate key too weak", 50 ms): the client applies OpenSSL's
>    SECLEVEL where HotSpot applies the JDK's trust-anchor rules. Filed as
>    `fixed-suite-bugs/tls-client-trust-is-openssl-seclevel-not-the-jdk-trustmanager-20260816-FIXED-20260817.md`.

## Reproducer

`TlsBoth.java` — H2's `NetUtils` on both ends in one process, timestamped:

```java
ServerSocket ss = org.h2.util.NetUtils.createServerSocket(port, true);
new Thread(() -> { try (Socket s = ss.accept()) { s.getInputStream().read(); } … }).start();
try (Socket c = org.h2.util.NetUtils.createLoopbackSocket(port, true)) {
    c.getOutputStream().write(7);          // forces the deferred handshake
}
```

```bash
cd apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
$JDK25/bin/java -cp "$CP:." TlsBoth 9431                                   # control
<cratonvm-bin> --java-home $JDK25 --nojit -c "$CP:." TlsBoth 9433          # subject
```

The cross-VM arms in the table above are the same two classes split into
`TlsServerOnly` / `TlsClientOnly`; splitting them is what showed that neither
leg is slow on its own.

## What this does NOT change

`TestTools` still fails at `TestTools.java:656`, the same line stock HotSpot 25
fails at, because H2 sets `javax.net.ssl.keyStore` and never a trust store
(upstream H2 skips `testSSL()` under `config.ci` for that reason). That was
already true when the page was written; this page was only ever about the
reporting difference at that shared failure point.

## Related
- `fixed-suite-bugs/h2-suite-bugs/bug-h2-netutils-dsa-privatekey-tls-unsupported-FIXED.md`
  — the legacy DSA server identity that makes this fixture use the OpenSSL
  fallback rather than rustls.
