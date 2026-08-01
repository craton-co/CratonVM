# `HostnameVerifier` consulted as a GATE instead of a FALLBACK — the JDK's own default verifier is a hardcoded `return false`, so every https request was rejected

**Status: RESOLVED (2026-08-01)**, branch
`fix/springboot-hostnameverifier-localhost-20260801`.

Retires BOTH of these OPEN docs, which are one defect:

- `docs/known-issues/springboot/simpleclienthttprequestfactory-app-hostnameverifier-rejects-localhost-20260731.md`
- `docs/known-issues/tomcat/tls-hostname-verification-regression-OPEN.md`
  (7 Tomcat classes, byte-identical exception message)

## Symptom

```
javax.net.ssl.SSLPeerUnverifiedException: Certificate for <localhost> does not match the installed HostnameVerifier
```

The TLS handshake itself succeeded — no trust-chain error, no
`SSLHandshakeException`. The failure came one step later, from an
application-level `HostnameVerifier.verify()` returning non-`true` for a
certificate whose SAN plainly covers `localhost`.

## Root cause

`f6028ba50` ("fix(natives): wave 2 — replace inline constant-closure stubs
with real code", 2026-07-28) made `HttpsURLConnection.set[Default]Hostname
Verifier` genuinely store the verifier, and wired
`http_url_connection::huc_verify_hostname` into the request path to *invoke*
it. That function invoked the installed verifier **unconditionally**, as an
additional gate every connection had to pass, and treated any non-`true`
answer as a rejection. It special-cased exactly one receiver as a no-op: the
VM's own bare-interface synthetic, class name `javax/net/ssl/HostnameVerifier`.

Two things are wrong with that, and both had to be fixed.

### 1. The real JDK's default verifier is `return false` — and it was NOT special-cased

`javax.net.ssl.HttpsURLConnection.<clinit>` installs
`HttpsURLConnection$DefaultHostnameVerifier`, and the constructor copies it
into every instance's `hostnameVerifier` field. `javap` on JDK 25:

```
class javax.net.ssl.HttpsURLConnection$DefaultHostnameVerifier implements javax.net.ssl.HostnameVerifier {
  public boolean verify(java.lang.String, javax.net.ssl.SSLSession);
    Code:
         0: iconst_0
         1: ireturn
}
```

It always answers `false`. Real JSSE never treats that as a policy decision:
`sun.net.www.protocol.https.HttpsClient.afterConnect` recognises this class
**by canonical name**, sets `setEndpointIdentificationAlgorithm("HTTPS")` on
the socket so the check happens inside the handshake, and never calls the
verifier at all.

**Why the original investigation could not close this.** The two ways to read
"the installed verifier" disagree on CratonVM, and only one of them is the one
that decides a request. Measured with `probes/HostnameVerifierDefaultProbe.java`:

| read | HotSpot | CratonVM (pre-fix) |
|---|---|---|
| `HttpsURLConnection.getDefaultHostnameVerifier()` (static) | `HttpsURLConnection$DefaultHostnameVerifier` → `false` | `javax.net.ssl.HostnameVerifier` (VM synthetic) → `true` |
| `connection.getHostnameVerifier()` (instance) | `HttpsURLConnection$DefaultHostnameVerifier` → `false` | `HttpsURLConnection$DefaultHostnameVerifier` → **`false`** |

`huc_hostname_verifier` reads the **instance** field first — so it resolved
the real JDK default and read its hardcoded `false` as a rejection. The
obvious probe (the static getter) returns the benign synthetic that WAS
special-cased, which is exactly why this looked like "some mystery concrete
verifier" rather than the JDK's own.

### 2. A `HostnameVerifier` is a FALLBACK in JSSE, never an extra gate

Even for a genuinely app-supplied verifier, `HttpsClient.checkURLSpoofing`
runs the built-in `HostnameChecker.match(host, peerCert)` FIRST and **returns
without ever calling the verifier when the name matches**. Only a *failed*
built-in check consults it, and a `true` answer there rescues the connection:

```java
    checker.match(host, peerCert);
    // if it doesn't throw an exception, we passed. Return.
    return;
} catch (SSLPeerUnverifiedException e) {
    // ignore
} ...
} else if ((hostnameVerifier != null) && (hostnameVerifier.verify(host, session))) {
    return;
}
```

So a `HostnameVerifier` on `HttpsURLConnection` can only ever WIDEN what is
accepted, never narrow it. The premise in the original doc comment — that the
gap being closed was "an app-supplied verifier that is STRICTER than the
default: certificate pinning, a CN/SAN allow-list" — is not how JSSE behaves;
such a verifier is not consulted at all while the hostname matches, on HotSpot
exactly as here.

## Resolution

`native-builtins/src/http_url_connection.rs`, `huc_verify_hostname` rewritten
to JSSE's actual shape:

1. **Built-in RFC 2818 endpoint identification first**, via the new
   `huc_builtin_endpoint_identification`, which delegates to the existing
   `x509_manager::verify_hostname` — the same matcher the `X509TrustManager`
   path uses, so the two cannot drift about what `localhost` matches. Pass →
   return, verifier never consulted.
2. **Only a failed built-in check consults the verifier**, and only when it is
   not a default stand-in. `is_default_hostname_verifier` now recognises the
   real JDK `javax/net/ssl/HttpsURLConnection$DefaultHostnameVerifier`
   alongside the VM's own bare-interface synthetic and an unnameable receiver.
3. Fail-closed on a throwing / non-`true` verifier is unchanged.

**This also closed a real hole rather than only restoring the old behaviour.**
The built-in check is re-derived here rather than assumed from the handshake,
because rustls only performs endpoint identification when it owns the
server-certificate policy. When the application supplies Java `TrustManager`s,
`t27_tls::PassthroughServerCertVerifier` is installed instead and ignores the
server name entirely, while `run_client_trust_check_for_chain` is a plain
`checkServerTrusted(chain, authType)` that does chain policy only. On that path
nothing in the VM was checking the hostname at all.

Also updated: the now-false "RESIDUAL, deliberately not papered over" comment
in `t27_tls.rs` above the `set{,Default}HostnameVerifier` registrations, and
the stale "additive to rustls's own RFC 6125 check" comment at the
`huc_verify_hostname` call site in `perform`.

## Guard tests

`native-builtins/src/http_url_connection.rs` (`http_url_connection_tests`):

- `jdk_default_hostname_verifier_is_recognised_as_a_non_check` — pins the JDK
  class name as a literal. This is a name-matching contract with the JDK (real
  JSSE matches the same class by canonical name), so a rename must break a test
  rather than silently fall through to "app verifier".
- `builtin_endpoint_identification_accepts_a_localhost_leaf` — the built-in
  check must accept the ordinary loopback case ON ITS OWN. If it ever returns
  `Err`, the JDK default's hardcoded `false` becomes the deciding answer again
  and every https request fails.
- `builtin_endpoint_identification_rejects_a_foreign_host_and_an_empty_chain`
  — and must reject, so the fallback to an app verifier stays reachable. A
  check that always passed would silently disable app verifiers altogether.

Both use `t27_certs/server.crt` (`CN=localhost`, SAN `dNSName=localhost` +
`IP 127.0.0.1`), the same shape as the Tomcat and Spring Boot test keystores.

`probes/HostnameVerifierDefaultProbe.java` reproduces the static-vs-instance
divergence above on demand, on either VM.

## Verification (A/B on two binaries, same host, 2026-08-01)

Baseline arm used `CratonVM-liquibase-scope-20260801`'s binary, confirmed
identical to `dev` for `http_url_connection.rs` and `t27_tls.rs`.

| | pre-fix | fixed |
|---|---|---|
| `SimpleClientHttpRequestFactoryBuilderTests` (Spring Boot) | 19 run, **2 failed** | **19/19 PASS** |
| `TestResolverSSL` (Tomcat) | `Tests run: 3, Failures: 1` | see table below |

With the fix, the debug trace shows the built-in check deciding it and the
verifier never being reached:

```
[dbg-tls-auth] huc_verify_hostname host="localhost" chain_len=1 builtin=Ok(())
```

(`CRATONVM_DBG=tls-auth` — note the grouped spelling; the old
`CRATONVM_DBG_TLS_AUTH=1` form is rejected at startup.)

All 7 Tomcat classes from the tomcat doc, fixed binary, one process per class:

```
  PASS         26,1s  hv=False  org.apache.catalina.valves.rewrite.TestResolverSSL
  PASS         28,7s  hv=False  org.apache.tomcat.util.net.TestCustomSslTrustManager
  PASS           18s  hv=False  org.apache.tomcat.util.net.TestSslHandshakeFailure
  PASS         51,1s  hv=False  org.apache.tomcat.util.net.TestSSLHostConfigCompat
  PASS         18,7s  hv=False  org.apache.tomcat.util.net.TestSSLHostConfigProtocol
  PASS         21,7s  hv=False  org.apache.tomcat.util.net.TestSSLHostConfigCipher
  FAIL        445,3s  hv=False  org.apache.tomcat.util.net.TestSsl
```

`hv=` is a grep of each log for the hostname-verifier message: **not one of the
7 logs contains it any more**, including the class that still fails.

`TestSslHandshakeFailure` is the informative pass: it asserts the exception
TYPE, and was reported failing with `expected<SSLHandshakeException> but
was<SSLPeerUnverifiedException>`. It now gets its `SSLHandshakeException`,
which confirms the fix restored the ordering rather than merely suppressing a
message.

`TestCustomSslTrustManager` is the other informative pass: it installs a
custom `TrustManager`, so it takes the `PassthroughServerCertVerifier` path
where rustls does NOT check the name. It passes on the strength of the new
built-in check — the path that previously had no hostname check at all.

`TestSsl` was also run on the baseline binary, because it is the one class
that still fails. Pre-fix it has **7** failures; post-fix **1**:

| | pre-fix | fixed |
|---|---|---|
| `testSimpleSsl[JSSE]` | `SSLPeerUnverifiedException` | PASS |
| `testSni[JSSE]` | `SSLPeerUnverifiedException` | PASS |
| `testKeyPass[JSSE]` | `SSLPeerUnverifiedException` | PASS |
| `testKeyPassFile[JSSE]` | `SSLPeerUnverifiedException` | PASS |
| `testSSLSessionTracking[JSSE]` | `SSLPeerUnverifiedException` | PASS |
| `testPost[JSSE]` | FAIL | PASS |
| `testClientInitiatedRenegotiation[JSSE]` | FAIL | FAIL (unchanged) |

`Tests run: 21, Failures: 7` → `Tests run: 21, Failures: 1`.

`testPost` is load-sensitive, not fixed-and-then-re-broken. Across the four
post-fix `TestSsl` runs it failed exactly once — the run that overlapped a
concurrent cargo build on this shared host — with thread-level
`java.io.IOException` (`os error 10053`, connection aborted) and a rejected
handshake, i.e. accept-path saturation in a test that fans out many concurrent
TLS POSTs. It passed in the other three. Class runtime swung 270–445s across
the same runs, which is the same load signal.

## Post-merge reverification

`dev` moved 113 commits between the fix and integration, so everything was
rebuilt and rerun on the merged tree (none of those commits touched
`http_url_connection.rs`, `t27_tls.rs` or `x509_manager.rs`):

- `SimpleClientHttpRequestFactoryBuilderTests` — 19/19 PASS, unchanged.
- All 6 other Tomcat classes — PASS, unchanged, still no verifier message.
- `TestSsl` — `Failures: 1` in 2 of 3 runs (the `testPost` flake above).

`TestSSLHostConfigCompat` was measured properly rather than reported from a
single run, because it failed once post-merge:

| arm | result |
|---|---|
| pre-fix | `Failures: 22` — **all 22** the verifier message, **0** read timeouts |
| post-fix, 5 runs | 3× `OK 78/78`, 2× `Failures: 1` (`testHostEC[JSSE-KEYSTORE]`, `Read timed out`) |
| stock HotSpot control | `OK (78 tests)` |

22 → 0/1. The residual `testHostEC` flake is **newly exposed, not newly
caused**: pre-fix the class never got past endpoint identification, so it could
not be observed, and this fix does no I/O — it cannot produce a socket read
timeout. Filed separately as
`docs/known-issues/tomcat/testsslhostconfigcompat-testhostec-read-timeout-20260801.md`
rather than folded in here or left implied by a green summary.

## Not fixed here, and not caused here

`TestSsl.testClientInitiatedRenegotiation[JSSE]` — a bare
`java.lang.AssertionError` from `Assert.assertTrue`, on TLS client-initiated
renegotiation. It fails **identically on the pre-fix baseline binary**, carries
no hostname-verifier message in either arm's log, and is not one of the
failures either OPEN doc described. Pre-existing and out of scope; left open
deliberately rather than folded into this fix.

