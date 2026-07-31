# `SimpleClientHttpRequestFactoryBuilderTests.connectWithSslBundle` — app-level `HostnameVerifier` now rejects a valid `localhost` peer

**Status: OPEN — found 2026-07-31**

## Symptom

`connectWithSslBundle(String)` [GET] and [POST] both fail:

```
JUnit Jupiter:SimpleClientHttpRequestFactoryBuilderTests:connectWithSslBundle(String):[1] httpMethod = "GET"
    => javax.net.ssl.SSLPeerUnverifiedException: Certificate for <localhost> does not match the installed HostnameVerifier
       javax.net.ssl.SSLException.<init>(SSLException.java:50)
       javax.net.ssl.SSLPeerUnverifiedException.<init>(SSLPeerUnverifiedException.java:53)
       org.springframework.http.client.SimpleClientHttpRequest.executeInternal(SimpleClientHttpRequest.java:89)
       org.springframework.http.client.AbstractStreamingClientHttpRequest.executeInternal(AbstractStreamingClientHttpRequest.java:87)
       org.springframework.http.client.AbstractClientHttpRequest.execute(AbstractClientHttpRequest.java:80)
       org.springframework.boot.http.client.AbstractClientHttpRequestFactoryBuilderTests.connectWithSslBundle(AbstractClientHttpRequestFactoryBuilderTests.java:119)
```

The TLS handshake itself succeeds (no `SSLHandshakeException`, no trust-chain
error) — the failure happens one step later, in an application-level
`HostnameVerifier.verify()` call that returns non-`true` for a certificate
whose Subject/SAN is expected to cover `localhost`.

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260731/all-jit/logs/module_spring-boot-http-client.org.springframework.boot.http.client.SimpleClientHttpReques-272f773888aa.{out,err}.log`

## Not a regression of the 2026-07-19 SSLBundle trust-validation fix

`docs/internal/fixed-suite-bugs/springboot/springboot-tls-sslbundle-trust-validation-gap-cluster-FIXED.md`
previously fixed this exact class/method for a *different* exception —
`SSLHandshakeException: handshake process: invalid peer certificate:
UnknownIssuer` (a trust-chain rejection) — and confirmed 32/32 PASS on
2026-07-19. That mechanism (item 3 in that doc's Resolution section: the
`https://` carrier + per-instance rustls `ClientConfig` capture) is still
intact here: trust validation now succeeds, and the test gets past the
handshake entirely. Today's failure is a **different, later-stage**
mechanism that could not even be reached before the trust bug was fixed —
so this is new exposure, not a recurrence of the old one.

## Root cause (confirmed trigger, not fully root-caused)

Commit `f6028ba50` ("fix(natives): wave 2 — replace inline constant-closure
stubs with real code", 2026-07-28) changed
`HttpsURLConnection.set[Default]HostnameVerifier` from a **discarding no-op**
to genuinely storing AND invoking the installed verifier
(`native-builtins/src/http_url_connection.rs`, `huc_verify_hostname`,
added in that commit; wired into the request path right after the
`TrustManager` check). Before that commit, no application-installed
`HostnameVerifier` was ever consulted, so a test like this one — which
apparently relies on (or the JDK/Spring plumbing installs) a concrete
`HostnameVerifier` rather than the VM's own bare-interface default — could
never fail this way; the check was silently skipped. `huc_verify_hostname`
explicitly special-cases only the VM's own synthetic default (class name
exactly `javax/net/ssl/HostnameVerifier`, see
`native-builtins/src/t27_tls.rs:4986`/`:5056`) as a no-op; any other
(concrete) verifier class is now genuinely invoked with a synthetic
`SSLSession` built from the just-captured peer chain
(`http_url_connection.rs:1994-2024`, `t27_tls::record_client_peer_chain`).

This means some concrete `HostnameVerifier` — not CratonVM's own bare
synthetic default — is installed on this connection (either by
Spring's `SimpleClientHttpsRequestFactory.prepareConnection`, by Spring
Boot's SSL bundle test scaffolding, or by real JDK bootstrap code) and its
`verify(host, session)` call is returning false/non-true for a
certificate that a normal handshake-level RFC 6125 check (done by rustls
itself, per this same function's doc comment) already accepted as valid for
`localhost`. Not root-caused further this session: it was not confirmed
whether the synthetic `SSLSession` handed to the verifier is missing data
the verifier's real bytecode needs (e.g. its `getPeerCertificates()`/SAN
extraction path), or whether a JDK-internal default verifier class (not
CratonVM's own bare-interface stand-in) is being resolved and running real
hostname-matching logic against data CratonVM populates incompletely.

## Affected classes

- `module/spring-boot-http-client` — `org.springframework.boot.http.client.SimpleClientHttpRequestFactoryBuilderTests` (`connectWithSslBundle`, both GET/POST parameterizations)

## Suggested next step

Add temporary logging in `huc_verify_hostname` to print the resolved
`verifier_cid`'s class name and the peer chain length/subject before the
`invoke_virtual("verify", ...)` call, then rerun this single test to see
which concrete verifier class is actually being invoked and what session
data it's working from.
