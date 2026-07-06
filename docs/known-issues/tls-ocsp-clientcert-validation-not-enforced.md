# TLS/mTLS/OCSP validation doesn't reject invalid handshakes (security-relevant)

**Status:** PARTIALLY FIXED (branch `fix/tls-ocsp-clientcert-validation-not-enforced-20260706`).
The core fail-open architecture gap is fixed and verified; several narrower,
pre-existing residuals were discovered along the way and are documented below,
not yet fixed. **Severity of what remains: reduced from high to
low/medium** — the remaining failures are fail-**closed** (reject a
connection that should succeed) or narrow feature gaps, not fail-**open**
(accept a connection that should be rejected), with one exception noted below.

**Related:** [BUG-DF06](../internal/CRATONVM_BUGS/BUG-DF06-certpath-pkix-not-implemented.md)
(FIXED — routed `CertPathValidator PKIX` to the real Sun SPI, a prerequisite
for the OCSP fix below) and
[BUG-DF02](../internal/CRATONVM_BUGS/BUG-DF02-stale-zeroed-oop-receiver-dispatch-segv.md)
(OPEN, unrelated memory-safety bug — not hit by any of this work).

## Root cause (confirmed)

CratonVM's rustls-based TLS engine (`native-builtins/src/t27_tls.rs`) built its
own certificate verifier directly from keystore/truststore PEM data and never
consulted the actual Java `TrustManager` objects passed to
`SSLContext.init(km, tms, random)`. Concretely, three independent gaps, all
now fixed:

1. **`SSLEngine.setEnabledCipherSuites()` was stored but never applied** to
   the rustls `ClientConfig`/`ServerConfig` — a connector's cipher restriction
   was purely cosmetic (only echoed back by `getEnabledCipherSuites()`).
2. **The Java `TrustManager[]` array was captured (for extracting trust-anchor
   PEM data) but the objects themselves were discarded** — so a
   revocation-aware `PKIXRevocationChecker` (OCSP/CRL, attached via
   `SSLUtilBase.getParameters()`/`PKIXBuilderParameters.addCertPathChecker`)
   or a fully custom `TrustManager` class (Tomcat's `trustManagerClassName`)
   was silently never invoked.
3. **A required-but-missing client certificate wasn't detected as a handshake
   failure on the client.** rustls's server-side `WebPkiClientVerifier`
   *did* correctly detect `NoCertificatesPresented` and abort — but
   `native-builtins/src/http_url_connection.rs`'s blocking TLS client driver
   (`perform()`) exits its handshake-wait loop as soon as
   `ClientConnection::is_handshaking()` flips false (which happens once the
   client sends its own TLS 1.3 Finished, *before* it can know the server will
   subsequently reject over the same connection), then silently folded the
   resulting "connection closed before response head" into
   `huc_real_perform`'s generic `Err(_) => Ok(-1)` contract instead of
   surfacing `SSLHandshakeException`.

## Fixes landed (verified)

- **Cipher-suite restriction is now enforced.** `t27_tls.rs`: new
  `cipher_provider_for`/`java_cipher_name_to_suite` build a `CryptoProvider`
  restricted to the Java-requested suites; `build_client_config_ciphers`/
  `build_server_config_single_cert_ex_ciphers` use it via
  `ClientConfig::builder_with_provider`/`ServerConfig::builder_with_provider`,
  wired into `engine_begin`.
- **Post-handshake `TrustManager` consultation.** `SSLContext.init` now
  captures the real `TrustManager[]` objects into a new, GC-rooted
  `ctx_trust_managers_table` (`t27_tls.rs`; root-scan/update wired into
  `vm/src/memory/roots.rs`/`gc.rs`). `EngineState` stores only a GC-stable
  `u64` key to this table (deliberately **not** the `ObjectRef`s themselves —
  `do_wrap`/`do_unwrap` call allocating helpers while holding
  `engine_registry()`'s write lock, so anything reachable through
  `EngineState` needing GC-root scanning would make that lock GC-relevant and
  risk a self-deadlock). `engine_take_pending_trust_check`/
  `engine_run_trust_check` call the real `checkClientTrusted`/
  `checkServerTrusted` once the handshake finishes (lock dropped first), and
  convert a thrown exception into `SSLHandshakeException`.
- **`PassthroughClientCertVerifier`** (`t27_tls.rs`): when Tomcat's
  `trustManagerClassName` mechanism is used (by design, no backing
  truststore), building `WebPkiClientVerifier` is impossible (no CA data).
  Detecting a registered custom `TrustManager` for the engine, the server
  config now uses a verifier that accepts any signed cert structurally and
  delegates the real trust decision entirely to the post-handshake check
  above — never used as a general fallback (would be fail-open) and only
  selected when the Java-side check is guaranteed to run.
- **`read_cert_der` (`x509_manager.rs`) now handles real certificate
  objects**, not just CratonVM's synthetic mirror shape — added a
  `getEncoded()` fallback. Without this, the chain built via
  `keystore::make_x509_mirror` looked "empty" to CratonVM's native
  `checkClientTrusted`/`checkServerTrusted` handler (registered on
  `sun.security.ssl.X509TrustManagerImpl` by class name), throwing
  `CertificateException: certificate chain is empty` for every connection —
  a regression this same session introduced and fixed before it shipped.
- **`TrustManagerFactoryImpl$PKIXFactory` registration** (`x509_manager.rs`):
  CratonVM only ever registered `engineInit`/`engineGetTrustManagers` on
  `TrustManagerFactoryImpl$SimpleFactory` (the "SunX509"-equivalent). Since
  this interpreter's native-override dispatch is keyed by the *receiver's
  runtime class*, and `PKIXFactory` (used for `TrustManagerFactory.getInstance
  ("PKIX")` — the default algorithm, and what Tomcat's
  `SSLUtilBase.getTrustManagers()` uses explicitly) doesn't override those
  methods, it ran as real, unintercepted JDK bytecode, producing a real
  `X509TrustManagerImpl` whose `checkClientTrusted`/`checkServerTrusted`
  (*still* natively overridden, by class name) found no `tm_registry` entry
  and silently fell back to a system-only trust state — rejecting any
  certificate signed by a private/test CA. This was invisible until this
  session's TrustManager-consultation fix started actually *calling*
  `checkClientTrusted` post-handshake; nothing did before. Added
  `tmf_engine_init_params`/`extract_pkix_trust_anchor_ders` (walks
  `CertPathTrustManagerParameters.getParameters()` → `PKIXParameters
  .getTrustAnchors()` → each `TrustAnchor.getTrustedCert()` → `.getEncoded()`)
  plus registrations for both `engineInit` overloads and
  `engineGetTrustManagers` on `PKIXFactory`, mirroring `SimpleFactory`'s.
- **`SSLHandshakeException` classification in the HTTPS client path**
  (`http_url_connection.rs`): a new `TLS_HANDSHAKE_FAILURE_SENTINEL`-prefixed
  error class distinguishes a TLS-handshake-phase failure (including the
  "closed with zero response bytes right after our own optimistic handshake
  completion" case above) from other connection failures, and
  `huc_real_perform` now raises `SSLHandshakeException` for it instead of
  silently returning `-1`.

### Verified test outcomes (JSSE variant; OpenSSL/OpenSSL-FFM variants
still skip — native `ssl.dll`/tomcat-native not installed, environmental,
unrelated)

| Class | Before this session | After |
|---|---|---|
| `TestSslHandshakeFailure` | 1/1 fail (fail-open: no cert, no exception) | **OK (1/1)** |
| `TestSecurity2017Ocsp` | 1/5 fail (fail-open) | **OK (5/5)** (unconfirmed stability — see below) |
| `TestOcspEnabled` | 20/116 fail | 15/116 fail (see residual #1) |
| `TestOcspSoftFail` | 3/15 fail | 2/15 fail |
| `TestOcspSoftFailInternalError` | 4/20 fail | 2/20 fail |
| `TestOcspSoftFailTryLater` | 4/20 fail | 2/20 fail |
| `TestClientCert` | **NOSUMMARY/hang** (never completed) | 13/18 pass, 5/18 fail (see residual #2) |
| `TestSSLHostConfigCipher` | 2/12 fail | 2/12 fail, unchanged (see residual #3) |
| `TestCustomSslTrustManager` | 2/9 fail | 2–3/9 fail, **order-dependent — see residual #4, needs attention** |

## Residuals (open, root-caused to varying depth — none are the original
fail-open bug; do not re-close this doc as fully fixed)

**1. `TestOcspEnabled`'s remaining 15/116 (and the ~2/class in the
`SoftFail*` variants) fail with "Handshake failed when not expected to do
so"** — a **fail-closed** regression surfaced (not introduced net-negative —
counts strictly improved) by the `PKIXFactory` fix above: even after that
fix, some parameter combinations still hit the system-only-trust-anchor
fallback. Not fully root-caused — candidates not yet ruled out: a *second*,
still-unregistered TrustManagerFactory entry point; possible thread-local
cross-contamination in `set_pending_tm_trust_roots`/
`attach_pending_identity_to_ctx` (a `PENDING_TM_TRUST_ROOTS` thread-local
written by one `TrustManagerFactory.init()` call and consumed by an unrelated
later `SSLContext.init()` on the same thread if the two aren't 1:1 — see
residual #4, which looks like the same family). `TestSecurity2017Ocsp`
passing cleanly is *not* strong evidence the mechanism is fully correct: its
single meaningful assertion (`testCVE_2017_15698`, a revoked client cert
should be rejected) would *also* "pass" if the trust check spuriously rejects
every connection — needs a dedicated positive-case regression test to confirm
it's testing what it claims.

**2. `TestClientCert`'s remaining 5/18 failures — "Checking requested client
issuer against ..."** — a **separate, deep, NOT fixed** architecture gap:
the client-side `X509ExtendedKeyManager.chooseClientAlias`/
`chooseEngineClientAlias` is never consulted at all. CratonVM's client TLS
path presents a fixed, pre-configured certificate directly
(`build_client_config`/`identity_override`) instead of calling back into Java
mid-handshake to let the real `KeyManager` pick a certificate matching the
server's `CertificateRequest` acceptable-issuer list. Fixing this requires a
custom `rustls::client::ResolvesClientCert` implementation that calls back
into Java *synchronously during the handshake* (unlike the post-handshake
`TrustManager` check above, this can't be deferred) — a materially riskier
change (GC/re-entrancy safety mid-handshake) that deserves its own dedicated
session. `TestCustomSslTrustManager`'s `testCustomTrustManagerCA` also hits
this same gap for its own issuer-check assertion.

**3. `TestSSLHostConfigCipher`'s 2/12 failures are unchanged** —
`testTls12CipherNotAvailable`/`testTls13CipherNotAvailable` still don't
reject: the **client**-side cipher restriction
(`TesterSupport.ClientSSLSocketFactory.setCipher()`, consumed through
`SSLSocketFactory.createSocket`, *not* the `SSLEngine` path this session's
cipher fix targeled) is a separate, narrower, not-yet-touched gap. The
**server**-side restriction (this session's fix) works correctly in
isolation — confirmed via a clean, non-timing-out standalone rerun (a batch
run reported a false "TIMEOUT" here that was shared-build-machine contention,
not a hang — reran alone in 27s).

**4. `TestCustomSslTrustManager` — needs follow-up, count is order-dependent.**
Isolated reruns consistently showed 2/9 failures
(`testCustomTrustManagerAll`/`CA`, both "connection closed immediately after
the TLS handshake with no response" — traced precisely to an SSLContext
*identity mismatch*: `attach_trust_managers_to_ctx` recorded the custom
`TrustManager` against one `SSLContext` object's GC-stable key, but
`createSSLEngine()` for the actual connection resolved to a *different*
context-key with no registered entry, so the `PassthroughClientCertVerifier`
path never activates and the original "`client_ca_pem` is `None`" config
error still fires). One later run (immediately after the `PKIXFactory` fix,
in a longer batch) showed **3/9** — `testCustomTrustManagerNone` newly
failing with `SSLHandshakeException: ... invalid peer certificate:
UnknownIssuer` instead of the expected `rc != 200` outcome via a config
error. This is consistent with the thread-local cross-contamination theory
in residual #1: a *stale* `PENDING_TM_TRUST_ROOTS` value (left over from an
unrelated `TrustManagerFactory.init()` earlier in the same test-class run,
possibly newly exercised by the `PKIXFactory` fix's added `engineInit`
registrations) being incorrectly claimed by this test's `SSLContext.init()`,
making `client_ca` non-empty when it should be `None`. **Not confirmed
deterministic — needs a dedicated, isolated investigation** before trusting
this test class's behavior. Recommended starting point: audit
`set_pending_tm_trust_roots`/`take_pending_tm_trust_roots` and
`attach_pending_identity_to_ctx` for a scenario where `TrustManagerFactory
.init()` fires without an immediately-following, corresponding
`SSLContext.init()` on the same thread (e.g. Tomcat's own config-validation
code calling `getTrustManagers()` standalone) — the thread-local has no
expiry/ownership check, so it will attach to whatever `SSLContext.init()`
happens next.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .suite\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS="1"; $env:CRATONVM_REAL_AQS="1"; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG="1"
.\cratonvm.exe -Xmx2g -cp $CP org.junit.runner.JUnitCore org.apache.tomcat.util.net.ocsp.TestOcspEnabled
# Set CRATONVM_DBG_TLS_AUTH=1 for verbose native-side tracing of the
# TrustManager-consultation pipeline (t27_tls.rs) added this session.
```

## Recommendation for follow-up sessions

1. Root-cause and fix the thread-local trust-root cross-contamination
   (residuals #1 and #4 both point here) — likely the single highest-value
   remaining fix, as it may explain most of the residual OCSP failures too.
2. Add a positive-case regression test alongside `TestSecurity2017Ocsp` (a
   *valid*, non-revoked client cert that must be accepted) to confirm the
   OCSP mechanism isn't just fail-closed-by-accident.
3. `TestClientCert`/`TestCustomSslTrustManager`'s issuer-check gap
   (client-side `KeyManager.chooseClientAlias` never consulted) is a
   substantial, separate feature — needs its own dedicated session given the
   mid-handshake re-entrancy/GC-safety considerations.
4. Client-side cipher-suite restriction (`SSLSocketFactory.createSocket` /
   `http_url_connection::perform`, not the `SSLEngine` path) is unfixed.
