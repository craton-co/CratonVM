# TLS/mTLS/OCSP validation doesn't reject invalid handshakes (security-relevant)

**Status:** OCSP revocation checking now IMPLEMENTED and verified (branch
`feat/ocsp-revocation-checking-20260706`, follow-up to
`fix/tls-residuals-followup-20260706` / the original
`fix/tls-ocsp-clientcert-validation-not-enforced-20260706` work). The
fail-open gap this doc previously tracked as its highest-priority open item —
`x509_manager::validate_chain` never checking revocation status at all — is
closed: real RFC 6960 OCSP request/response handling, wired through the real
`java.security.cert.PKIXRevocationChecker` API, now runs as "Step 7" of chain
validation. `TestSecurity2017Ocsp` is back to 5/5 (this time because
`testCVE_2017_15698`'s revoked cert is correctly rejected, not because of an
accidental unknown-issuer failure), `TestOcspEnabled` is 116/116 (the
residual 5 `serverOk=false,verifyServer=true` failures are gone), and all
three `TestOcspSoftFail*` classes are fully green. See "OCSP revocation
checking implementation" below for what was built and what's still a
documented simplification (CRL support). Residuals #2 (`chooseClientAlias`)
and #3 (client-side cipher restriction) remain untouched, confirmed still
open — see "Residuals still open" below, unchanged from prior sessions.

**Related:** [BUG-DF06](../internal/CRATONVM_BUGS/BUG-DF06-certpath-pkix-not-implemented.md)
(FIXED) and
[BUG-DF02](../internal/CRATONVM_BUGS/BUG-DF02-stale-zeroed-oop-receiver-dispatch-segv.md)
(OPEN, unrelated memory-safety bug — not hit by any of this work).

## This session's fixes (verified)

Root-caused via a fast, isolated Java repro (~1s per iteration, `/tmp/tlsrepro`
on the Azure host — a standalone client+`HttpsServer`/mutual-TLS program
against the same test keystores, instead of the ~4min full Tomcat suite) once
`CRATONVM_DBG_TLS_AUTH=1` tracing on the real suite showed the failure was
100% reproducible and parameter-independent, not the intermittent
thread-local race the prior session's residuals #1/#4 hypothesized.

1. **`keystore::keystore_id_from_object`** (`keystore.rs`, new): a real
   `java.security.KeyStore` wrapper object has no room for CratonVM's
   `cratonvm$keystore$storeId` pseudo-field (`set_field_by_name` silently
   no-ops on a real bytecode class's undeclared field) and no reliable slot-4
   fallback (the real 4-field layout — `type`/`provider`/`keyStoreSpi`/
   `initialized` — has different semantics per index). The ONLY tier that
   actually resolves it is the identity-hash side table
   `keystore.rs::store_id_by_identity` already built for exactly this reason
   (see its own doc comment) — but `x509_manager::read_keystore_id` and
   `tls::read_keystore_registry_id` each reimplemented a subset of the lookup
   and never consulted that table (`tls.rs`'s own doc comment flagged this
   exact gap and asked for exactly this helper — it just hadn't been written
   yet). Result before this fix: `TrustManagerFactory.init(KeyStore)`
   *always* resolved keystore id 0 for a caller-supplied truststore, silently
   validating peers against the ~120 platform root certs instead of the
   caller's actual (test/private) CA — any peer cert signed by that CA was
   rejected with `UnknownIssuer`, unconditionally, regardless of OCSP/cipher/
   client-cert configuration. Both callers now delegate to the new helper.
2. **`x509_manager::get_tm_id`/`set_tm_id`** (identity-hash side table added,
   mirroring `keystore.rs`'s pattern exactly): the SAME bug, one level up.
   `TrustManagerFactory.getInstance("PKIX")` in real-JDK mode really does
   return a real `sun.security.ssl.TrustManagerFactoryImpl$PKIXFactory` SPI
   instance (confirmed by direct pointer tracing), and
   `set_tm_id(ctx, this, id)` — called from `tmf_engine_init`/
   `tmf_engine_init_params` to stamp the id this factory was just built
   with — hit the identical named-field/slot-0 no-op problem. The very next
   call, `tmf_engine_get_trust_managers`, read back id **0** from the *same
   object pointer* that had just been stamped with a real, non-zero id one
   line earlier. Every `TrustManager` returned by `getTrustManagers()`
   therefore carried id 0; `do_check_trusted`
   (`checkClientTrusted`/`checkServerTrusted`) resolved that to
   `build_trust_manager_state(0)` (platform roots only) and rejected any
   chain signed by the caller's actual CA with "no trust anchor found for
   chain" — this is what a mutual-TLS repro (server validating a *valid*
   client cert) surfaced directly.
3. **`x509_manager::register_trust_manager_state`/`trust_manager_state_by_id`**
   (new, `x509_manager.rs`): a real but distinct bug from #1/#2 — TWO
   independent id counters (`x509_manager`'s own `tm_registry`/`next_tm_id`,
   and `keystore.rs`'s keystore-registration counter) both start at 1 and
   were being stamped onto the *same* `cratonvm$x509tm$id` field by different
   producers (`tls.rs`'s `getTrustManagers()` stamped a raw keystore id;
   `x509_manager.rs`'s `tmf_engine_get_trust_managers` stamped a `tm_registry`
   id), while consumers (`tls::validate_cert_chain`,
   `phases_late::p68_extract_trust_manager_roots`) uniformly called
   `build_trust_manager_state(id)`, which only knows how to interpret `id` as
   a keystore id. A `tm_registry`-sourced id would either resolve to nothing
   (empty anchors, fail-closed) or — worse — to a numerically-coincident,
   totally unrelated keystore. Fixed by unifying both producers onto the
   `tm_registry` id space and both consumers onto a single
   `trust_manager_state_by_id` lookup.

None of the above is the thread-local (`PENDING_TM_TRUST_ROOTS`) mechanism
residuals #1/#4 blamed — that relay was, on inspection, working correctly for
the sequential, single-threaded call patterns these tests exercise. It's
still fragile in the abstract (no ownership/expiry check) and worth hardening
if it ever causes a *confirmed* incident, but it was not the cause here — the
real bugs were the two identity/id round-trip gaps above, both now closed.

## OCSP revocation checking implementation (this session, closes the prior "New finding")

`x509_manager::validate_chain` previously performed full structural PKIX
validation (chain continuity, expiry, `BasicConstraints`, name constraints,
signature verification, trust-anchor matching) but never checked revocation
status at all — `TrustManagerState.enable_crl` was a field that got set
(always to `false`) and was never read anywhere in the crate. That gap is now
closed with a real implementation, not a stub:

- **`TrustManagerState.enable_crl: bool` replaced by
  `revocation: Option<RevocationConfig>`.** `RevocationConfig` (new struct)
  carries everything read off a real, caller-attached
  `java.security.cert.PKIXRevocationChecker`: an explicit OCSP responder URI
  override (`getOcspResponder()`), an explicit pinned responder certificate
  (`getOcspResponderCert()`), and the `Option` set (`ONLY_END_ENTITY`,
  `PREFER_CRLS`, `NO_FALLBACK`, `SOFT_FAIL`). `extract_revocation_config`
  (`x509_manager.rs`, called from `tmf_engine_init_params`) walks
  `CertPathTrustManagerParameters.getParameters().getCertPathCheckers()`,
  identifies a `PKIXRevocationChecker` by walking the object's real class
  hierarchy (not by hard-coding the internal impl class name — confirmed via
  `javap` against the real JDK that `getRevocationChecker()` returns
  `sun.security.provider.certpath.RevocationChecker`, but the code matches on
  the public `PKIXRevocationChecker` supertype instead), and reads its
  configuration purely through its real public getters
  (`invoke_virtual`) — never raw field access, consistent with this file's
  hard-learned lesson (see the identity-hash-side-table pattern used
  elsewhere in this crate: field-poking a real bytecode object silently
  no-ops or misreads).
- **`ParsedCert` gained two fields**: `serial_der` (the raw DER `INTEGER`
  content of `tbsCertificate.serialNumber`, needed byte-exact for OCSP
  `CertID.serialNumber`) and `ocsp_responder_uri` (the first `id-ad-ocsp`
  `accessLocation` URI found in the certificate's Authority Information
  Access extension, OID `1.3.6.1.5.5.7.1.1` — neither AIA nor
  CRLDistributionPoints was previously parsed anywhere in this file). This is
  how the responder URL is discovered when no explicit override is
  configured — matching exactly how the Tomcat test suite's
  `TesterOcspResponderServlet` is discovered in practice (no test ever calls
  `setOcspResponder`/`setOcspResponderCert`; every cert's AIA extension
  points at `http://127.0.0.1:8888`).
- **`validate_chain` gained "Step 7"**: when the active `TrustManagerState`
  carries a `RevocationConfig`, every certificate in the chain except the
  matched trust anchor (and, when `ONLY_END_ENTITY` is set, every cert except
  the leaf) is checked via a real OCSP round-trip: a DER `OCSPRequest`
  (RFC 6960 §4.1.1, SHA-1 `CertID` — the RFC's own conventional default, and
  what every responder including the test harness's actually expects) is
  POSTed over plain HTTP (OCSP responders are conventionally unencrypted;
  hand-rolled single-shot HTTP/1.1 client rather than reusing
  `http_client.rs`/`http_url_connection.rs`, which are considerably heavier
  machinery — TLS, redirects, connection pooling — built for the
  `java.net.http`/`HttpURLConnection` surface, not a same-process byte-in/
  byte-out POST), the DER `OCSPResponse` is parsed down to a
  `BasicOCSPResponse`, and **its signature is verified** before any status is
  trusted — either against an explicitly pinned responder cert, or against a
  delegated responder cert embedded in the response's own `certs` field
  (checked for the `id-kp-OCSPSigning` EKU and for being signed by the same
  issuing CA, per RFC 6960 §4.2.2.2 — this is the exact trust model the test
  harness's `TesterOcspResponderServlet` uses), or directly against the
  issuing CA's key. A definite `revoked` status always rejects the chain,
  regardless of `SOFT_FAIL`; an indeterminate outcome (responder unreachable,
  timeout, malformed response, non-`successful` `OCSPResponseStatus` such as
  `tryLater`/`internalError`, or a `certStatus` of `unknown`) rejects unless
  `SOFT_FAIL` is set, in which case validation continues — soft-fail only
  ever covers *failure to obtain an answer*, never a positive revoked
  verdict.
- **A second wiring point**: the native `HttpURLConnection` client path
  (`http_url_connection.rs`, via `t27_tls::build_engine_client_config_with_identity`)
  builds its own rustls `ClientConfig` directly and never calls back into
  Java's `X509TrustManager.checkServerTrusted` — so it never reached
  `validate_chain` on its own, and a `PKIXRevocationChecker`-configured
  `TrustManagerFactory` used only for
  `HttpsURLConnection.setDefaultSSLSocketFactory` would have silently kept
  accepting revoked server certs even after the `SSLEngine`/`TrustManager`
  path above was fixed. `t27_tls.rs` now carries the `RevocationConfig`
  alongside the scoped trust roots (`set_pending_tm_revocation`,
  `TlsTrustRoots.revocation`) and installs a new
  `OcspAwareServerCertVerifier` — a `rustls::client::danger::ServerCertVerifier`
  that delegates structural validation to the normal `WebPkiServerVerifier`
  and then runs the same OCSP check (`x509_manager::check_revocation_for_verifier`)
  against the presented chain — whenever a `RevocationConfig` is present.
- **Documented simplification: CRL is not implemented.** `PREFER_CRLS` and
  `NO_FALLBACK` are both read out of the real `PKIXRevocationChecker` and
  stored, but since there is no CRL fetch/parse, both options currently
  collapse to "OCSP only" (a `NO_FALLBACK`-without-`PREFER_CRLS` config
  behaves identically to the same config without `NO_FALLBACK`, since there's
  no CRL fallback to suppress in the first place). This is a real, deliberate
  gap, not an oversight — OCSP is the path this Tomcat test suite actually
  exercises (no test in `TestOcspEnabled`/`TestOcspSoftFail*`/
  `TestSecurity2017Ocsp` sets `PREFER_CRLS` or relies on CRL), so it was
  prioritized over CRL fetch-and-parse, which would need its own
  DER `CertificateList`/`TBSCertList` parser and its own signature
  verification path. A future session adding real CRL support should extend
  `RevocationConfig`'s handling in `check_revocation`
  (`x509_manager.rs`) rather than introduce a parallel mechanism.

### Verified test outcomes (JSSE variant; OpenSSL/OpenSSL-FFM variants still
skip — native `ssl.dll`/tomcat-native not installed, environmental, unrelated)

| Class | Before OCSP session | After OCSP implementation |
|---|---|---|
| `TestSslHandshakeFailure` | OK (1/1) | not rerun this session — no code touched on this path, no reason to expect a change |
| `TestSecurity2017Ocsp` | 1/5 fail (`testCVE_2017_15698`, revocation-checking gap) | **OK (5/5)** — revoked client cert with a CVE-2017-15698-shaped long AIA URI is correctly rejected |
| `TestOcspEnabled` | 5/116 fail, all `serverOk=false,verifyServer=true` | **OK (116/116)** |
| `TestOcspSoftFail` | 1/15 fail | **OK (15/15)** |
| `TestOcspSoftFailInternalError` | 2/20 fail | **OK (20/20)** |
| `TestOcspSoftFailTryLater` | 4/20 fail | **OK (20/20)** |
| `TestClientCert` | 13/18 pass, 5/18 fail | 13/18 pass, 5/18 fail — unchanged (spot-checked), same known `chooseClientAlias` gap (residual #2, still open, unrelated to revocation checking) |
| `TestSSLHostConfigCipher` | 2/12 fail | not rerun this session — no code touched here, no reason to expect a change (residual #3, still open) |
| `TestCustomSslTrustManager` | 2/9 fail, consistently | 2/9 fail, consistently (spot-checked) — same known `chooseClientAlias` gap, unrelated to revocation checking |
| `native-builtins` unit tests (`x509_manager` module) | 39 tests | **48 tests, all pass** — 9 new tests covering OCSP DER request encoding, response parsing (good/revoked/unknown/tryLater), SHA-1 known-answer vector, and signature accept/reject |

Regression check: `TestClientCert` + `TestCustomSslTrustManager` run together
produced exactly 27 tests / 7 failures — matching the pre-existing baseline
(18+9 tests, 5+2 failures) exactly, confirming the OCSP work introduced no
regression in the unrelated `chooseClientAlias` gap.

## Residuals still open (unchanged from before this session)

**Residual #2 — client-side `KeyManager.chooseClientAlias` never
consulted.** `TestClientCert`'s 5/18 and `TestCustomSslTrustManager`'s
`testCustomTrustManagerCA`/`All` (2/9). CratonVM's client TLS path presents a
fixed, pre-configured certificate instead of calling back into Java
mid-handshake to match the server's `CertificateRequest` acceptable-issuer
list. Needs a custom `rustls::client::ResolvesClientCert` calling back into
Java *synchronously during the handshake* — a materially riskier change
(GC/re-entrancy safety mid-handshake) than anything in this session; still
recommended as its own dedicated session.

**Residual #3 — client-side cipher-suite restriction unfixed.**
`TesterSupport.ClientSSLSocketFactory.setCipher()`, consumed through
`SSLSocketFactory.createSocket`/`http_url_connection::perform`, is a separate
code path from the `SSLEngine`-based cipher restriction fixed previously.
Not rerun this session (no code touched here).

## Reproduction

```bash
# On the Azure host (victor@20.83.144.174), or equivalent Linux box with a
# real JDK (pass --java-home explicitly — see the JDK-detection note below):
cd /data/data/apps/tomcat  # or wherever the compiled Tomcat test tree + cp.txt live
CP=$(cat .suite/cp.txt)
CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  ./cratonvm --java-home /path/to/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.tomcat.util.net.ocsp.TestOcspEnabled
# Set CRATONVM_DBG_TLS_AUTH=1 for verbose native-side tracing of the
# TrustManager-consultation pipeline (t27_tls.rs / x509_manager.rs) — this
# session added tracing to tmf_engine_init/tmf_engine_get_trust_managers/
# do_check_trusted/validate_cert_chain, in addition to what already existed.
```

**Environment note (unrelated to the TLS bug, but blocks all testing until
understood):** on a fresh Linux host, if `JAVA_HOME`/`java` aren't visible in
the *exact* environment the process runs under (common for non-interactive
SSH commands — an interactive shell's `JAVA_HOME` doesn't propagate),
`vm/src/config.rs::detect_real_jdk()` silently falls back to CratonVM's
synthetic-JDK mode, which has a stale 2-field `StringBuilder` layout
(pre-dating JDK 9 compact strings) — this throws `NoSuchFieldError` on
*any* JDK9+-compiled invokedynamic string concatenation, even a trivial
`"a" + "b"`. Always pass `--java-home` explicitly on a host you haven't
verified. Separately, `sun/nio/ch/FileKey.init` was only registered for the
Windows-shaped `(FileDescriptor, int[])` overload — a real Unix JDK's
`FileKey` (confirmed via `javap`) uses `(FileDescriptor, long[2])` filling
`st_dev`/`st_ino`; this was missing entirely and caused an
`UnsatisfiedLinkError` on the first `FileChannel.lock()`/`tryLock()` in
real-JDK mode on Linux (blocked `OcspBaseTest`'s responder lock file).
Both fixed this session (`native-io/src/file_channel.rs`).

## Recommendation for follow-up sessions

1. ~~Implement real OCSP/CRL revocation checking~~ — **DONE this session**
   (OCSP; see "OCSP revocation checking implementation" above). Real CRL
   fetch/parse remains a documented gap (`PREFER_CRLS`/`NO_FALLBACK` currently
   collapse to "OCSP only") — worth its own follow-up if a workload actually
   exercises CRL-preferring configurations, but no test in this suite does.
2. `TestClientCert`/`TestCustomSslTrustManager`'s issuer-check gap
   (client-side `KeyManager.chooseClientAlias` never consulted) — substantial,
   separate feature, needs its own dedicated session (mid-handshake
   re-entrancy/GC-safety).
3. Client-side cipher-suite restriction (`SSLSocketFactory.createSocket` /
   `http_url_connection::perform`, not the `SSLEngine` path) is unfixed.
4. If `get_km_id`/`set_km_id` (KeyManager id, `x509_manager.rs`) is ever
   implicated in a bug report, apply the same identity-hash-side-table fix as
   `get_tm_id`/`set_tm_id` preemptively — it's presumably the same gap, just
   not yet hit by a failing test.
5. A companion Linux native-registration gap was found (and is being fixed in
   a separate, parallel session) while iterating on this feature's test
   suite: `sun/nio/ch/UnixDispatcher.close0`/`preClose0` are missing native
   registrations, surfacing as `UnsatisfiedLinkError` during NIO
   `ServerSocket.close()` teardown (e.g. `TestOcspTimeout`'s `@AfterClass`
   responder-shutdown hook) — did not block any of this session's target
   suites (none of them hit the affected teardown path), but is a real,
   separate bug; see whichever branch/commit closes it for details.
