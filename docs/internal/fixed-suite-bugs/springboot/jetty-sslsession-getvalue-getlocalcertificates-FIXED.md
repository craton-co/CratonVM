# `module/spring-boot-jetty`: `SSLSession.getValue`/`getLocalCertificates` gaps broke every HTTPS request through `SecureRequestCustomizer` — FIXED

**Status: FIXED — 2026-07-24.** Closes 2 of the 3 residual classes from the
2026-07-23 rerun (`apps/spring-boot-suite-runner/RESULTS-20260723.md`'s
3-residual count for `module/spring-boot-jetty`), reproduced fresh against
current `dev` in worktree `CratonVM-spring-boot-jetty-closure-20260723`,
branch `fix/spring-boot-jetty-full-closure-20260723`, binary
`cratonvm-spring-boot-jetty-closure.exe`.

## Symptom 1 — `AbstractMethodError` on every HTTPS request

```
java.lang.AbstractMethodError: method javax/net/ssl/SSLSession.getValue(Ljava/lang/String;)Ljava/lang/Object; has no Code attribute
	at org.eclipse.jetty.server.SecureRequestCustomizer.retrieveSni(SecureRequestCustomizer.java:242)
	at org.eclipse.jetty.server.SecureRequestCustomizer.checkSni(SecureRequestCustomizer.java:222)
	at org.eclipse.jetty.server.SecureRequestCustomizer.customize(SecureRequestCustomizer.java:203)
```

Jetty's `SecureRequestCustomizer` calls `SSLSession.getValue()`/`putValue()`
on every HTTPS request (`retrieveSni()`, `getX509()` — both cache their
result on the session via this API). `javax.net.ssl.SSLSession` is a real
JDK interface; a synthetic object impersonating it has no bytecode to fall
back to for an unregistered method, so any un-intercepted call throws
`AbstractMethodError`. `getValue`/`putValue`/`removeValue`/`getValueNames`
were never registered anywhere in the crate at all — every HTTPS request
through a `SecureRequestCustomizer`-driven connector failed with a 500,
affecting `JettyServletWebServerFactoryTests` (HANG — see "Interaction with
the Xerces/TLD-scan residual" below) and every SSL test in
`JettyReactiveWebServerFactoryTests` (8/35 FAIL).

### Fix

`native-builtins/src/t27_tls.rs`: registered the 4 methods on
`javax/net/ssl/SSLSession`, backed by a real `java.util.HashMap` lazily
allocated and stored in the session object's own **last field** (both
synthetic session shapes — `build_synthetic_ssl_session`'s 7-field
engine-session and the legacy `SSLServerSocket.accept()` path's 3-field
session — were bumped by one slot to hold it). Storing the map as an actual
object field rather than a native side-table keyed by object address means
normal GC root-scanning of the (live, reachable) session object keeps
arbitrary attribute *values* alive for free, sidestepping the whole class of
GC-unstable-identity-key bugs this file's other side tables (`sslparams_alpn_table`,
`session_peer_certs_table`) have hit historically.

`getValueNames()` cannot just return `HashMap.keySet().toArray()` directly —
`Collection.toArray()` reifies as `Object[]`, not `String[]`, and the method's
own descriptor declares `[Ljava/lang/String;`. Real JDK's own
`SSLSessionImpl.getValueNames()` has the identical mismatch and copies into a
freshly-typed array; this fix does the same (allocate via the same generic
`ClassId::new(0)` "untyped ref array" convention already used elsewhere in
this file for `String[]` returns, then copy each element across).

## Symptom 2 — `400 Bad Request: Invalid SNI` on every HTTPS request (exposed by fixing Symptom 1)

Fixing `getValue` let every request past `retrieveSni()`, only to immediately
hit `checkSni()`'s call to `getX509()`:

```java
private X509 getX509(SSLSession session) {
    X509 x509 = (X509) session.getValue("org.eclipse.jetty.server.x509");
    if (x509 == null) {
        Certificate[] local = session.getLocalCertificates();
        if (local == null || local.length == 0 || !(local[0] instanceof X509Certificate)) {
            return null;   // <- checkSni() throws HttpException.RuntimeException(400, "Invalid SNI") on this
        }
        x509 = new X509(null, (X509Certificate) local[0]);
        session.putValue("org.eclipse.jetty.server.x509", x509);
    }
    return x509;
}
```

`SSLSession.getLocalCertificates()` **was already registered** (from an
earlier, narrower fix — see the "netty-client-socket-write-after-close
residual" comment still in `phases_late.rs`), but hardcoded to always return
`null`, with a comment explicitly scoped to "a client session [that] never
presents a certificate (no mTLS in this path)". That assumption is wrong for
a **server** session: a TLS server always presents its own certificate, and
`getLocalCertificates()` from the server's own side means exactly that
certificate. Every HTTPS request through `SecureRequestCustomizer` — 100% of
them, regardless of client-auth configuration, since `getX509()` runs
unconditionally — got a 400 the moment `getValue`'s crash stopped masking it.

### Fix

`native-builtins/src/phases_late.rs`'s `getLocalCertificates` registration
now looks up a real local-certificate-chain side table
(`t27_tls::local_certs_for_session`, backed by a new `session_local_certs_table`,
same keying convention as the existing peer-certs table) instead of
hardcoding `null`. `t27_tls.rs`'s `build_synthetic_ssl_session` populates it
at session-construction time from the engine's own identity: its
`identity_override` (the per-`SSLContext` cert configured via a `KeyManager`
— used for both a server's own identity and an mTLS client's presented
identity) if set, else the process-global `runtime_tls_identity()` for a
server engine with no per-context override (a plain client with no
configured identity correctly still gets an empty chain — preserving the
original comment's intent for that specific case).

## Verified

Full `module/spring-boot-jetty` (15 classes) rerun,
`RunName=jetty-closure-verify2-20260724`, `-TimeoutSec 1500`:

| Class | Before this fix | After |
|---|---:|---:|
| `JettyReactiveWebServerFactoryTests` | 8 FAIL / 35 (`AbstractMethodError`/400) | **1 FAIL** (already-tracked AssertJ `representation` NPE, unrelated — see `micrometer-tracing-opentelemetry-assertj-representation-npe-and-eventpublisher-residuals.md`) |
| `JettyServletWebServerFactoryTests` | HANG at 300s (25 FAIL / 113 once given a 1500s budget) | 832.7s, **12 FAIL** (all the same already-tracked AssertJ NPE) |

The other 13 classes in the module were unaffected (12 PASS, 1 —
`JettyServletWebServerServletContextListenerTests` — hits a separate,
already-tracked cross-cutting Mockito issue; see
`tomcatservletwebserverservletcontextlistenertests-mockito-forkedclasspath-mockmethodadvice.md`).

`JettyServletWebServerFactoryTests`'s HANG was never actually an infinite
hang: the class's remaining, still-open Xerces/TLD-scan slowness residual
(`jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals-FIXED.md`'s
final section) means the class needs roughly 800-1100s to run all 113 tests,
well past the suite runner's 300s per-class default — it was being
misclassified as HANG purely from budget exhaustion, not a real infinite
wait. Confirmed genuinely running to completion given a larger budget.

## Reproduce / rerun

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "<worktree>\target\release\cratonvm-spring-boot-jetty-closure.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <module/spring-boot-jetty class list tsv> `
  -RunName <name> -Parallel 3 -TimeoutSec 1500
```
