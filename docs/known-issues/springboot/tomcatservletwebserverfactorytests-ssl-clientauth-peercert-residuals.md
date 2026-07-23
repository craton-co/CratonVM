# `TomcatServletWebServerFactoryTests` — SSL client-auth / peer-certificate residuals — OPEN

**Status: OPEN — found 2026-07-23**, while giving this class a long timeout
to distinguish genuine hangs from the known embedded-Tomcat
throughput-wall pattern (see `apps/spring-boot-suite-runner/RESULTS-20260723.md`
and the sibling `TomcatWebServerFactoryCustomizerTests`, which *does* just
need more time and has no real defect). This class is **not** pure
throughput: it completes in 980-1230s (well under a 1500s timeout, so not a
hang) but has real test failures beyond the throughput-wall pattern.

**Two real, distinct `AsynchronousServerSocketChannel` bugs found and fixed
in the same commit as this doc** (both were previously masking
`sslWithHttp11Nio2Protocol`'s real behavior):

1. `bind(SocketAddress, int)` (the two-arg, explicit-backlog overload Tomcat's
   `Nio2Endpoint.bind` calls) had no native registration — only the one-arg
   form did — so real-JDK dispatch hit the abstract method directly and threw
   `AbstractMethodError: has no Code attribute`. Fixed by registering the
   same handler for both descriptors (`native-io/src/async_socket.rs`).
2. `getLocalAddress()`/`localAddress()` had no native registration at all,
   so `Nio2Endpoint.getLocalPort()` (`((InetSocketAddress) serverSock
   .getLocalAddress()).getPort()`) NPE'd immediately after a successful
   bind. Fixed by adding `aio_assc_local_address`. **First attempt at this
   fix introduced a real deadlock-shaped regression**: it read the bound
   address via `listener.lock().local_addr()` on the *same* `Mutex` the
   accept worker thread holds for the entire duration of its blocking
   `TcpListener::accept()` call (see `Job::Accept` in the same file) — since
   `getLocalPort()` fires immediately after `start()`, right as the accept
   loop begins, this could (and did, once — reproduced as a full
   suite-timeout HANG) block indefinitely waiting for the first connection.
   Fixed properly by capturing the `SocketAddr` once at bind time (before
   any accept loop exists) and storing it directly on `AioHandle::Listener`,
   so `getLocalAddress()` never touches the contended `Mutex` at all.

With both fixes in place, `sslWithHttp11Nio2Protocol`'s embedded Tomcat
connector now binds and starts successfully (previously it never got past
`start()`) — but the test still fails, now with a plain client-side
handshake **timeout** (`SSLHandshakeException: ... os error 10060`, Windows'
"no response received in the required time") rather than a VM-level defect.
Not yet root-caused whether this is a genuine remaining gap in the NIO2
accept/SSL-handshake integration or host-load flakiness (this session ran
many concurrent long builds/test runs on a shared box) — worth a clean,
isolated rerun before assuming either way.

The other failures (SSL client-auth handshake / peer-certificate issues)
remain open and are documented below, unfixed.

## Reproduction

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe <binary> -JdkHome $env:JAVA_HOME `
  -ClassList <tsv with module/spring-boot-tomcat org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests> `
  -RunName repro -Parallel 1 -TimeoutSec 1500
```
Needs a long timeout (this class alone took ~1121s / ~19 min under CratonVM)
to get past the default 300s suite timeout and actually observe these
failures instead of a misleading TIMEOUT/HANG classification.

## Failure 1: client-cert mutual-TLS handshake outright rejected

`sslWantsClientAuthenticationSucceedsWithClientCertificate(File)` and
`sslNeedsClientAuthenticationSucceedsWithClientCertificate(File)` (the
`ClientAuth.WANT` and `ClientAuth.NEED` variants — the latter seen
intermittently across reruns this session, not confirmed to fail every
time; worth confirming it's not host-load flakiness before assuming it's
identical to the `WANT` case) — server configured to request/require a
client certificate, client presents one:

```
javax.net.ssl.SSLHandshakeException: connection closed by peer during
handshake (likely a rejected handshake, e.g. no cipher suite in common)
    org.apache.hc.client5.http.ssl.AbstractClientTlsStrategy.executeHandshake(...)
```

Only `TLSv1.2` is requested (`getSsl(ClientAuth.WANT, "password",
"classpath:test.jks", null, new String[] { "TLSv1.2" }, null)` —
`AbstractServletWebServerFactoryTests.java:667-683`), no unusual cipher
suite — so this is NOT the already-documented CBC/DHE gap (both permanent,
both about specific cipher suites this test doesn't request). The handshake
is rejected specifically when the *client* attaches its own certificate/key
material (`SSLContextBuilder...loadKeyMaterial(keyStore, ...)`) to present
during the handshake — the sibling test with `ClientAuth.WANT` but no
client-side key material presented (see Failure 2) gets *past* the
handshake, pointing at something in CratonVM's rustls-backed server TLS
config specifically mishandling the client's certificate offer during
`ClientAuth.WANT` (optional) negotiation.

## Failure 2 & 3: handshake succeeds, but the client can't see the server's peer certificate afterward

`sslWantsClientAuthenticationSucceedsWithoutClientCertificate()` (no client
cert offered) and `shouldUpdateSslWhenReloadingSslBundles()` (SSL bundle
hot-reload, unrelated to client-auth) both fail *after* the handshake
completes, inside the test's own hostname verifier:

```
java.lang.IllegalStateException: peer not authenticated (no certificate in session)
    TomcatServletWebServerFactoryTests$RememberingHostnameVerifier.verify(...)
    org.apache.hc.client5.http.ssl.AbstractClientTlsStrategy.verifySession(...)
```

`RememberingHostnameVerifier` (test-local class,
`TomcatServletWebServerFactoryTests.java:783`) calls something like
`session.getPeerCertificates()` to inspect the *server's* certificate chain
as part of hostname verification — from the client side, "peer" here means
the server. This throwing `SSLPeerUnverifiedException`-shaped
"peer not authenticated" after an apparently-successful handshake suggests
`SSLSession.getPeerCertificates()` on CratonVM's client-side session isn't
populated with the server's certificate chain in this scenario, even though
the connection otherwise appears to proceed (the exception fires from the
verifier callback mid-handshake-completion, not from a connection-refused
error).

## Working hypothesis (unconfirmed)

Both symptoms could be two faces of the same gap: CratonVM's rustls server
config, when built with `ClientAuth.WANT` (optional — `AllowAnyAnonymousOrAuthenticatedClient`-shaped
in rustls terms) rather than `NONE` or `NEED`, may not be constructing/
exposing the certificate chain the same way rustls does for a plain
unauthenticated server config — outright rejecting the handshake when the
client actually offers a cert (Failure 1), and/or failing to surface the
server's own chain into the session that the JSSE-facing `SSLSession`
wrapper exposes to `getPeerCertificates()` (Failures 2 & 3, both of which
use `ClientAuth.WANT`-configured servers — confirm `shouldUpdateSslWhenReloadingSslBundles`'s
SSL config before trusting this fully, not verified this session). Not
confirmed — needs a from-scratch repro isolating `ClientAuth.WANT` server
config + `SSLSession.getPeerCertificates()` from the rest of Tomcat/Spring
Boot's plumbing, the same methodology the CBC-suite doc
(`../../internal/rustls-cbc-cipher-suites-not-supported.md`) used
successfully to get past Tomcat's own exception-swallowing `LifecycleBase`
logging.

## Next steps for whoever picks this up

1. Reproduce `ClientAuth.WANT` + `getPeerCertificates()` directly against
   CratonVM's `native-builtins` TLS layer (`t27_tls.rs` and friends),
   bypassing Tomcat, to confirm/refute the shared-root-cause hypothesis
   above.
2. If confirmed: check how `t27_tls.rs` builds the rustls `ServerConfig`
   for `ClientAuth.WANT` (likely `WebPkiClientVerifier::builder(...).allow_unauthenticated().build()`
   or similar) versus `NONE`, and how the resulting session's certificate
   chain gets surfaced back through the `SSLSession`/`SSLEngine` Java-facing
   wrapper.
3. If refuted (two unrelated bugs): split this doc, since Failure 1 (handshake
   rejection) and Failures 2-3 (peer-cert session gap) would need separate
   investigations.

## Affected classes

| Module | Class | Method |
|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` | `sslWantsClientAuthenticationSucceedsWithClientCertificate`, `sslWantsClientAuthenticationSucceedsWithoutClientCertificate`, `sslNeedsClientAuthenticationSucceedsWithClientCertificate` (intermittent), `shouldUpdateSslWhenReloadingSslBundles`, `sslWithHttp11Nio2Protocol` (handshake timeout residual after the two `AsynchronousServerSocketChannel` fixes above — not yet confirmed real vs. host-load flakiness) |

Not confirmed elsewhere in the suite this session — scope limited to this
one class's residuals found while long-timeout-verifying the
`module/spring-boot-tomcat` 5-class batch.
