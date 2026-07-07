# TLS/mTLS/OCSP validation doesn't reject invalid handshakes (security-relevant)

**Windows `openssl_h` `Optional.or` NPE (`task_c068bce2`) — root-caused and
FIXED 2026-07-07.** The symptom reported when this task was opened:
```
INFO [...] Starting test case [test[OpenSSL-FFM with OpenSSL trust ...]]
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError
  class=org/apache/tomcat/util/openssl/openssl_h$OpenSSL_version_num
  cause=java/lang/NullPointerException Cannot invoke "java.util.Optional.or(java.util.function.Supplier)"
```
**Root cause:** two competing native registrations existed for
`java/lang/foreign/SymbolLookup`. `panama.rs::register_pe_symbol_lookup` is
the correct implementation — it actually attempts a real library load via
`ctx.load_native_library` and wraps results in a genuine `Optional`/
`Optional.empty()`. But that function ran under the registry's default
`SyntheticStub` category, which is dropped entirely under strict-no-stubs
(the default for real-JDK mode) — so it silently never registered at all.
The only surviving registration was a duplicate stub in
`phases_late.rs::register_p67_foreign_memory` (category `Bridge`, never
dropped) that unconditionally faked success (`libraryLookup` always
allocated a "loaded" object regardless of whether any library existed) and
`find` always returned a bare Java `null` instead of `Optional.empty()`.
Real JDK bytecode composes lookups via `SymbolLookup.or()`, whose generated
lambda does `this.find(name).or(() -> other.find(name))` — a `null`
receiver there is exactly the observed NPE. Confirmed via direct tracing
(temporary `eprintln!` instrumentation) that the `phases_late.rs` stub, not
`panama.rs`'s real implementation, was firing.

Empirically (repro via `run-tomcat-suite.ps1`, `TestClientCert`/
`TestSecurity2017Ocsp`/`TestSSLHostConfigCipher`/`TestOcspEnabled`), this NPE
was already being caught gracefully by
`OpenSSLLifecycleListener.isAvailable()`'s `catch (Throwable t)` — it did
**not** actually kill the whole test class as originally suspected; all
parameterized sub-tests (18/116/etc.) ran to completion both before and
after the fix. The original "all 8 classes fail/hang" framing traces to a
stale build predating several since-merged dev commits, not to this NPE
itself. It was still a real, worth-fixing bug: HotSpot's actual behavior for
a missing OpenSSL library is a clean `IllegalArgumentException("Cannot open
library: ...")` thrown directly from `SymbolLookup.libraryLookup`, not an
NPE from a downstream `.or()` composition.

**Fix:** promoted `register_pe_symbol_lookup` to the `Bridge` category (so
it survives strict-no-stubs) and deleted the redundant/broken duplicate
trio (`loaderLookup`/`libraryLookup`/`find`) from `phases_late.rs`. Verified
post-fix: `openssl_h`'s `<clinit>` now fails with
`IllegalArgumentException Cannot open library: ssl.dll` (matching real JDK
semantics) instead of the `Optional.or` NPE, across `TestClientCert`,
`TestSecurity2017Ocsp`, and `TestSSLHostConfigCipher`. All 61
`native-builtins` panama:: unit tests still pass.

**`KeyManagerFactory.getProvider()` NoSuchMethodError/NPE — root-caused and
FIXED 2026-07-07 (`task_0ec4b356`).** Was blocking every JSSE-variant
sub-test across this cluster (100% failure rate; OpenSSL/OpenSSL-FFM
variants unaffected) with:
```
Caused by: java.lang.NoSuchMethodError: java/lang/String.getInfo()Ljava/lang/String;
  at org.apache.tomcat.util.net.SSLUtilBase.getKeyManagers(SSLUtilBase.java:351)
```
(`kmf.getProvider().getInfo().contains("FIPS")`.)

**Root cause, part 1 (wrong receiver type):** the ACTIVE `KeyManagerFactory`
registration — `phases_late.rs::register_p68_ssl`'s `getInstance`/`init`
(NOT `tls.rs::register_key_manager_factory`, which is a fully-shadowed dead
duplicate registered under `SyntheticStub` that never fires; confirmed via
direct tracing that only the `phases_late.rs` copy ever runs) — stored the
raw `algorithm` *String* argument at field slot 0. `getProvider()` has no
native override, so it falls through to real bytecode, which reads the REAL
`javax.net.ssl.KeyManagerFactory` field layout. `javap -p` on the actual
class shows the true declaration order is `provider` (slot 0),
`factorySpi` (slot 1), `algorithm` (slot 2) — easy to get backwards from a
naive constant-pool read, which surfaces `factorySpi` first purely because
another method happens to reference it first. So `getProvider()` returned
the algorithm String sitting at slot 0, and `.getInfo()` on a `String`
receiver is exactly the observed `NoSuchMethodError`.

**Root cause, part 2 (wrong Provider construction), found after fixing part
1:** once `getProvider()` correctly returned a `Provider`-tagged object, its
`.getInfo()` call returned `null` instead of a string, NPEing on
`.contains(...)`. `alloc_concurrent_synthetic("java/security/Provider", N)`
upsizes to the *real* class's full field count in real-JDK mode — which
includes every inherited `Hashtable`/`Properties` field ahead of
`Provider`'s own `name`/`info`/`version`/... — so a naive indexed
`ctx.set_field(provider, 0/1, ...)` (mirroring the simpler, legacy
`phases_early.rs::make_provider` 2-field convention) lands on whatever
field occupies that slot in the *real* inheritance layout, not `name`/
`info`. `native-builtins/src/jca/provider_chain.rs` already solves this
correctly (its own module doc explains the exact same trap, WP6.1) by
writing every field through `set_field_by_name` and registering its own
`getInfo()` native that reads `info` back the same way — confirmed via an
isolated repro that `Security.getProvider("SunJSSE").getInfo()` already
worked correctly through that path.

**Fix:** in `phases_late.rs`, `KeyManagerFactory.getInstance` now builds its
`provider` field via `jca::provider_chain::find("SunJSSE")` +
`jca::provider_chain::make_provider(...)` (both promoted from private to
`pub(crate)`) instead of an ad-hoc allocation, and stores `algorithm` at
slot 2 / leaves `factorySpi` unset at slot 1, matching the real class
layout; `getAlgorithm()`'s native override was updated to read slot 2
accordingly; `init(KeyStore, char[])` no longer clobbers slots 1/2 (nothing
ever read the keystore/password it used to stash there — the actual mTLS
identity flow runs through `keystore_set_pending_km_identity`, untouched).
Added a doc comment to `tls.rs::register_key_manager_factory` flagging it
as dead/shadowed so a future reader debugging `KeyManagerFactory` doesn't
lose time editing the copy that never runs.

Verified: the isolated repro
(`KeyManagerFactory.getInstance("SunX509").getProvider().getInfo()`) now
returns a real info string and `.contains("FIPS")` evaluates `false` as
expected. Through the full suite: `TestSecurity2017Ocsp` is back to **5/5
PASS** on Windows. `TestClientCert`'s JSSE sub-tests no longer hit the
`getProvider` error at all — they now fail later, during the actual TLS
handshake (`SSLHandshakeException: connection closed by peer`), which lines
up with this doc's already-documented, separate residual #2
(`chooseClientAlias`/client-cert plumbing, still open) rather than a new
issue. `TestSSLHostConfigCipher` improved from 4/12 to 3/12 failing (residual
#2-shaped JSSE failures, not the DHE case this doc already tracks as
permanently unfixable). **Do not expect Windows counts to match this doc's
Linux baseline** — residual #2 and the DHE case are real, independent,
already-tracked gaps, not artifacts of either fix on this page.

**`nb_tls_tmf_get_trust_managers_propagates_keystore_id` — root-caused and
FIXED 2026-07-07 (`task_2b28e7bc`): stale test, not a production bug.**
Confirmed via `git stash` that this test failed identically before either
fix on this page, so it wasn't a regression from this session — but it also
wasn't a real correctness bug in `TrustManagerFactory`. The test (added by
an earlier `nb-tls-tmf` commit) asserted that `getTrustManagers()`'s
returned `X509TrustManager` carries the *raw keystore registry id* `init()`
bound (slot 0 == 7, the test's simulated bound id). A **later**, deliberate
fix (`FIX (tls-residuals)`, see `tls.rs::register_trust_manager_factory`'s
`getTrustManagers()` and `x509_manager.rs`'s `register_trust_manager_state`/
`trust_manager_state_by_id` doc comments) intentionally changed that
contract: stamping the raw keystore id directly caused
`validate_cert_chain` to misresolve it against a numerically-coincident but
unrelated `tm_registry` entry from a different `TrustManagerFactory` SPI
path (both id counters start at 1 and increment independently). The fix
instead builds a `TrustManagerState` from the bound keystore id and
registers *that state* through `register_trust_manager_state`, stamping the
resulting `tm_registry`-scoped id (which starts at 1 in a fresh process —
exactly the `Int(1)` the test was seeing instead of `Int(7)`). The test was
simply never updated to match this intentional, well-documented behavior
change.

**Fix:** rewrote the test's final assertion to check the *actual* invariant
that matters — that resolving the stamped id via
`x509_manager::trust_manager_state_by_id` yields a `TrustManagerState` whose
own `keystore_id` field equals the bound id (7), i.e. the indirection
resolves correctly — rather than asserting raw numeric equality with the
bound id. No production code changed. Verified: the test now passes; the
full `tls::` suite (80 tests) and `x509_manager::` suite (48 tests) both
pass with no regressions.

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
documented simplification (CRL support).

Separately (branch `fix/tls-client-cipher-restriction-20260706`), residual
#3 (client-side cipher-suite restriction) is now **CLOSED for the TLS 1.3
case** and confirmed **permanently unfixable for the TLS 1.2 DHE case** — see
"Residual #3 closeout" below.

Residual #2 (`chooseClientAlias` never consulted) is now **IMPLEMENTED and
verified via a direct standalone repro** (branch
`fix/tls-clientcert-resolver-v2-20260706`) — a real `rustls::client::
ResolvesClientCert` calls back into the actual Java `KeyManager.
chooseClientAlias`/`getPrivateKey` synchronously mid-handshake, matching the
server's requested issuer exactly. **However**, `TestClientCert`/
`TestCustomSslTrustManager` could not be re-verified at 18/18 and 9/9: both
are blocked by (1) the classloading/vtable-install deadlock the cipher-
restriction session above already found and documented (reproduces under
this fix too, with the identical signature, at a higher frequency — this
fix's extra first-time class loads make the pre-existing race easier to
hit, not a new bug), and (2) a newly-discovered, separate, more fundamental
gap: Tomcat's connector never actually sets `need`/`want` client auth to
`true` for any connection either target test class makes in this
environment, so the server never sends a `CertificateRequest` at all,
regardless of the client-side fix. See "Residual #2 implementation" below for
the full picture, including the direct-repro evidence that the fix itself is
correct.

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

## OCSP revocation checking implementation (closes the prior "New finding")

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
| `TestSslHandshakeFailure` | OK (1/1) | OK (1/1) — confirmed no regression (one batch run showed a flake, isolated rerun passed cleanly, matching this suite's known shared-host batch-contention behavior) |
| `TestSecurity2017Ocsp` | 1/5 fail (`testCVE_2017_15698`, revocation-checking gap) | **OK (5/5)** — revoked client cert with a CVE-2017-15698-shaped long AIA URI is correctly rejected |
| `TestOcspEnabled` | 5/116 fail, all `serverOk=false,verifyServer=true` | **OK (116/116)** |
| `TestOcspSoftFail` | 1/15 fail | **OK (15/15)** |
| `TestOcspSoftFailInternalError` | 2/20 fail | **OK (20/20)** |
| `TestOcspSoftFailTryLater` | 4/20 fail | **OK (20/20)** |
| `TestClientCert` | 13/18 pass, 5/18 fail | 13/18 pass, 5/18 fail — unchanged (spot-checked), same known `chooseClientAlias` gap (residual #2, still open, unrelated to revocation checking) |
| `TestSSLHostConfigCipher` | 2/12 fail | **1/12 fail** (residual #3 closed for TLS 1.3; `testTls12CipherNotAvailable`'s DHE case is permanently unfixable — see "Residual #3 closeout") — fixed in the separate `fix/tls-client-cipher-restriction-20260706` session, not the OCSP work |
| `TestCustomSslTrustManager` | 2/9 fail, consistently | 2/9 fail, consistently (spot-checked) — same known `chooseClientAlias` gap, unrelated to revocation checking |
| `native-builtins` unit tests (`x509_manager` module) | 39 tests | **48 tests, all pass** — 9 new tests covering OCSP DER request encoding, response parsing (good/revoked/unknown/tryLater), SHA-1 known-answer vector, and signature accept/reject |

Regression check: `TestClientCert` + `TestCustomSslTrustManager` run together
produced exactly 27 tests / 7 failures — matching the pre-existing baseline
(18+9 tests, 5+2 failures) exactly, confirming the OCSP work introduced no
regression in the unrelated `chooseClientAlias` gap.

## Residual #2 — client-side `KeyManager.chooseClientAlias` consultation IMPLEMENTED (branch `fix/tls-clientcert-resolver-v2-20260706`), verified via direct repro; `TestClientCert`/`TestCustomSslTrustManager` end-to-end verification blocked by two separate, pre-existing issues

**What was built:** a real `rustls::client::ResolvesClientCert` (`t27_tls::JavaKeyManagerResolver`) replaces the fixed, pre-configured client identity `build_client_config`/`build_engine_client_config_with_identity` previously always presented. When the server's `CertificateRequest` arrives mid-handshake, `resolve()` now:

1. Renders the server's DER-encoded acceptable-issuer hints (`root_hint_subjects`) to RFC 4514 DN strings via a new `security_manager::x509::parse_name_dn` (reusing the existing hand-rolled ASN.1 DN renderer, previously only used for JAR-signer policy matching) and wraps each in a synthetic `X500Principal` (same 1-field/string convention `X509Certificate.getSubjectX500Principal()` already used).
2. Maps the server's offered `SignatureScheme`s to JSSE `keyType` strings (`"RSA"`/`"EC"`/`"Ed25519"`/`"Ed448"`).
3. Calls the *real* Java `KeyManager.chooseClientAlias(keyType[], issuers[], socket)` via `ctx.invoke_virtual` on the actual `KeyManager` object(s) captured from `SSLContext.init()` (new `ctx_key_managers_table`, mirroring the existing `ctx_trust_managers_table` pattern exactly, including its GC-root-scan/remap companions) — this is what makes a test-installed wrapper like Tomcat's `TrackingKeyManager` actually observe the call and record the requested issuers, and what would let a genuinely custom `KeyManager` have real say.
4. Calls `getPrivateKey(alias)` on the same (possibly-wrapped) `KeyManager`, decodes the `km_id` this VM already packs into the returned key's identity composite (`x509_manager::km_id_from_private_key_mirror`, a new accessor), and reads the cert chain + PKCS#8 key DER directly out of the existing `km_registry` (`km_alias_material`, new) — avoiding a second Java round-trip for `getCertificateChain`, since it's the same registry `choose_client_alias`'s own native implementation already consults.
5. Builds a `rustls::sign::CertifiedKey` from that DER directly (no PEM round-trip) and hands it to rustls.

**Fixed alongside (both real, both hit immediately once `chooseClientAlias`/`getPrivateKey` were actually invoked for the first time):**
- **`x509_manager::get_km_id`/`set_km_id`** had the identical identity-hash-side-table gap this doc's own "Recommendation for follow-up sessions" #4 predicted for `get_tm_id`/`set_tm_id`'s twin: a real bytecode `KeyManagerFactoryImpl$SunX509`/`SunX509KeyManagerImpl` instance has no room for the `cratonvm$x509km$id` pseudo-field, so `kmf_engine_init`'s stamp and `kmf_engine_get_key_managers`'s very next read (on the same pointer) silently round-tripped to 0 — every `KeyManager` returned by `getKeyManagers()` would have resolved to km_id 0, and `choose_client_alias`/`get_private_key` would have missed `km_registry` entirely the moment they were actually called. Fixed identically to `get_tm_id`/`set_tm_id` (an identity-hash-keyed `km_id_by_identity` side table, mirrored exactly).
- **GC safety of the new `ctx_key_managers_table`** (holds live `ObjectRef`s across calls, same as its `ctx_trust_managers_table` sibling): scan/remap companions wired into `vm/src/memory/roots.rs`/`gc.rs` next to the existing trust-manager entries.
- **Re-entrancy**: `resolve()` is called by rustls synchronously, mid-handshake, from deep inside `process_new_packets()` — with no way for rustls's fixed trait signature to pass a `NativeContext` through. Solved with a thread-local raw pointer (`t27_tls::set_active_native_context`/`with_active_native_context`), published by an RAII guard immediately before the handshake-driving code that might call `resolve()` and cleared (even on early return) before the underlying `&mut dyn NativeContext` borrow could dangle — the same "erase the lifetime, rely on an RAII guard to bound its true validity" pattern this codebase already uses for other native side-tables, just applied to a borrow instead of a heap object.
- **Renegotiation-shaped resolve() calls**: initially the fix only wrapped the explicit handshake loop, but `has_certs()`/`resolve()` can also fire from *inside* `read_response`'s `stream.read()` calls (rustls handles a server-triggered mid-connection re-handshake transparently inside `StreamOwned`'s `Read`/`Write` impls) — widened the active-context window (and removed all `begin_blocking_region()`/`end_blocking_region()` wrapping) to cover the *entire* https exchange in `http_url_connection::perform`, not just the initial loop, for exactly this reason.
- **GC-blocked-region hazard**: `resolve()` allocates and runs bytecode, which cannot safely happen while this thread is marked GC-parked (`begin_blocking_region()`) — `perform()`'s https branch no longer opens one at all (a loopback exchange is fast enough that never GC-parking here is an acceptable trade-off); the TCP connect and the plain (non-TLS) branch still do, since those are pure byte I/O with no Java interaction.

**Verified independently, via a fast (~1s/iteration) standalone repro** (`/tmp/tlsrepro/ClientCertRepro.java` on the Azure host — a `com.sun.net.httpserver.HttpsServer` with `SSLParameters.setNeedClientAuth(true)` set upfront, a client wrapping `KeyManagerFactory.getKeyManagers()` in a `TrackingKeyManager`-shaped class, both using the same Tomcat test keystores) that the fix works end-to-end:

```
chooseClientAlias called! keyTypes=[EC, Ed25519, RSA] issuers=1
  requested issuer: CN=Apache Tomcat Test CA, OU=Apache Tomcat PMC, O=The Apache Software Foundation, L=Wilmington, ST=DE, C=US
  -> chosen alias: user1
getPrivateKey(user1) -> present, algo=RSA
```
— matching the server's actual CA subject exactly — followed by the server-side trace confirming a real, complete, *trusted* mTLS handshake: `do_unwrap ... is_handshaking=false peer_certs_present=2`, `engine_run_trust_check: 1 trust manager(s), method=checkClientTrusted, chain_len=2`, `do_check_trusted ... registry_hit=true`, `invoke_virtual[0] -> Ok`. This is direct, unambiguous evidence that `chooseClientAlias` consultation, issuer-DN rendering, alias selection, private-key retrieval, and the resulting certificate are all correct and accepted by a real peer.

**Why `TestClientCert`/`TestCustomSslTrustManager` could not be re-verified at 18/18 and 9/9 despite the above:** two separate, pre-existing issues block the *Tomcat* integration harness specifically (neither is a regression from this fix, and neither is specific to `chooseClientAlias`):

1. **The classloading/vtable-install lock-ordering deadlock this doc's `fix/tls-client-cipher-restriction-20260706` session already found and documented** (see "Residual #3 closeout" above and follow-up item #7) reproduces under this fix too, with the identical `gdb` signature (`parking_lot::raw_rwlock::RawRwLock::lock_exclusive_slow` inside `vtable_install_adapter`/`define_class_with_options`, no other thread visibly holding the lock) — triggered via several different call shapes across repeated runs (`resolve_field_ref`, `Class.forName`, exception-handler lookup, plain `invokestatic`-driven class loading during Tomcat's own server startup, i.e. *not* specific to this fix's new class usage). A baseline (unmodified) binary run 4/4 times clean in the same window; this fix's binary hung roughly half the `TestClientCert` attempts and all 3 `TestCustomSslTrustManager` attempts — consistent with "this fix's extra first-time class loads (`X500Principal`/`Principal`/array types) raise the probability of hitting a pre-existing race," not with introducing a new one. Pre-warming those specific classes (and their array types) in the safe, top-level `perform()` call context reduced but did not eliminate the hang. Left as-is rather than chased further: this is VM-core locking code, already flagged by a sibling session as needing its own dedicated concurrency-debugging session, and duplicating that investigation here would not be a good use of either session's time.
2. **A separate, newly-discovered gap: Tomcat's connector never actually sets `need`/`want` client auth to `true` for *any* connection in either target test class**, in this environment. Exhaustive tracing (`CRATONVM_DBG_TLS_AUTH=1`) across an entire `TestClientCert` run (11 engines, covering both the "preemptive" and "renegotiation-on-protected-resource" code paths) showed `setSSLParameters id=N need=false want=false` and `engine_begin request=false client_ca_none=true` for *every single one*, including the `testClientCertGetWithPreemptive` case that should request a cert upfront. Since the server-side `SSLEngine` is never told to request a certificate, it never sends a `CertificateRequest`, `JavaKeyManagerResolver::resolve` is (correctly) never invoked for these specific connections, and the client presents nothing — the `SSLHandshakeException: connection closed ... likely rejected (missing client cert)` failure for all 5 `TestClientCert` JSSE tests and 2 `TestCustomSslTrustManager` JSSE tests is the direct result, and is **identical in a from-scratch unmodified build** (confirmed via a concurrent OCSP-session log, `client_cert.log`, showing the exact same 5/18 failure signature on code that has never touched `chooseClientAlias`). This is *not* the issuer-check-assertion-failure shape this doc previously described for residual #2 (which implied a successful handshake with a merely-untracked issuer) — that description appears to have been based on code inspection rather than an observed run, and does not match current behavior. The true root cause is upstream of the client entirely: something in how Tomcat's `SSLHostConfig`/`preemptiveAuthentication`/`SSLAuthenticator` machinery is supposed to toggle `SSLEngine.setNeedClientAuth`/`setWantClientAuth` (directly, or via `SSLParameters`) per-connection or via mid-stream renegotiation is not reaching the native engine in this environment. Confirmed **not** a client-cert-resolver-specific gap: a minimal repro using `SSLParameters.setNeedClientAuth(true)` set upfront on a plain `HttpsServer` (bullet above) works perfectly, proving the underlying `need_client_auth` → `CertificateRequest` → `resolve()` pipeline is sound when actually configured. Needs its own dedicated session, focused on Tomcat's `AbstractEndpoint`/`SSLHostConfig`/`SSLAuthenticator` → CratonVM SSLEngine configuration path (`net_phase_e.rs`/`t27_tls.rs`'s `setSSLParameters`/`setNeedClientAuth`/`engine_begin` handlers), *not* on `chooseClientAlias` itself.

**Regression check:** `TestClientCertTls13` (JSSE variant, both `testClientCertGet`/`testClientCertPost`) and `TestSslHandshakeFailure.testMissingClientCertificate` — the closest adjacent suites that also exercise the native `HttpsURLConnection`/`http_url_connection.rs` client path — pass cleanly (7/7) against this fix, matching pre-existing behavior; `JavaKeyManagerResolver::resolve` was not invoked in that run either (these specific tests don't appear to depend on it), consistent with no regression. A full-suite regression sweep beyond these adjacent classes was not completed given the shared-host instability described above (concurrent sessions actively rewriting the same files, a `/data` volume remount mid-session that broke every absolute path in every worktree simultaneously, and the pre-existing vtable-install race making repeated runs unreliable) — recommend re-running the broader Tomcat suite once the shared host is quiet and residual VM-core locking bug is fixed.

**Environment note:** the Azure host's `/data` volume was remounted mid-session (an extra `/data/data/data/...` nesting level appeared under everything that used to be `/data/data/...`, including `jdk25-real`, `cratonvm`, and every `wt-*` worktree) — every absolute path baked into `.suite/cp.txt`, git worktree admin links, and this doc's own prior reproduction commands broke simultaneously. `.suite/cp.txt` needs regenerating (or path-substituting) after a remount like this; a worktree's `.git` administrative link can also get pruned if the directory was briefly "missing" during the remount, requiring a fresh `git worktree add` + copying working-tree changes across rather than a simple `git worktree repair`.

## Residuals still open

**Residual #3 — client-side cipher-suite restriction: CLOSED for TLS 1.3,
permanently open for TLS 1.2 DHE (branch
`fix/tls-client-cipher-restriction-20260706`).**

`TestSSLHostConfigCipher` went from 2/12 failing to 1/12 (only
`testTls12CipherNotAvailable[JSSE]`, the DHE case — see below). The actual
fix required three separate discoveries, none of which matched this doc's
own prior assumption about the code path:

1. **The entire NEW13 `SSLContext`/`SSLSocketFactory`/`SSLSocket`
   implementation in `phases_late.rs` (`register_p68_ssl`) is dead code in a
   real-JDK build.** It's gated behind `#[cfg(feature = "synthetic-jdk")]`
   (see `vm/src/native/builtins.rs`'s no-op shims for when that feature is
   off), which is OFF in the standard build every test in this doc runs
   under. An initial attempt to fix cipher restriction by wiring up
   `SSLSocketFactory.createSocket`/`SSLSocket.setEnabledCipherSuites` there
   compiled clean and changed nothing at runtime — confirmed by adding a
   debug print directly in the handler and observing zero hits. The
   *actually live* `SSLContext`/`SSLSocketFactory` implementation for a
   real-JDK build is `net_phase_e.rs::register_re6_ssl_context`.
2. **`TestSSLHostConfigCipher`'s HTTPS request never goes through
   `SSLSocketFactory.createSocket` at all** — `TomcatBaseTest.getUrl()` uses
   `HttpURLConnection`, whose real request path
   (`http_url_connection::huc_real_perform`/`ensure_connected`, both calling
   the shared `perform()`) builds its own rustls connection directly from
   native-side globals (`t27_tls::huc_default_client_identity`/
   `active_client_trust_roots`, populated when `SSLContext.getSocketFactory()`
   is called) and never creates a Java-visible `SSLSocket` or calls
   `setEnabledCipherSuites` on one. `TesterSupport.ClientSSLSocketFactory`'s
   `createSocket()`/`reconfigureSocket()` override — the only place cipher
   restriction is ever expressed — is simply never invoked. Fixed by adding
   `http_url_connection::huc_upcall_create_socket_if_custom_factory`, which
   reads `HttpsURLConnection.defaultSSLSocketFactory` (the real JDK static
   field — read directly rather than intercepting the setter, because
   `setDefaultSSLSocketFactory` is real, non-native JDK bytecode that the
   interpreter's native-override-priority rules let win, so a native
   override on it never fires) and, when appropriate (see below),
   `invoke_virtual`s the factory's real `createSocket(host, port)` —
   genuine Java bytecode, so `ClientSSLSocketFactory`'s override actually
   runs, including its call to `setEnabledCipherSuites`. The resulting
   socket's backing stream id (from `net_phase_e.rs`'s `SockSide` table) is
   read directly and handed to `perform()` via a new
   `established_https_stream_id` parameter, bypassing `perform()`'s own
   connect entirely for this case. `SSLSocket.setEnabledCipherSuites` itself
   is a new live registration in `net_phase_e.rs` (the old one was also
   inside the dead `phases_late.rs` module): since the socket's handshake
   already completed unrestricted inside `createSocket`, honoring a
   restriction means tearing down and reconnecting via
   `t27_tls::build_engine_client_config_with_identity_ciphers` (a new
   cipher-restricting variant of the existing
   `build_engine_client_config_with_identity`).
3. **`t27_tls::rustls_client_connect`'s handshake loop had a real,
   independent, previously-latent bug**: `read_tls` returns `Ok(0)` — not an
   `Err` — when the peer closes the connection (rustls's documented
   contract; its own examples all check for a zero return). The loop ignored
   the return value entirely, so once cipher restriction could actually
   cause a server to reject and close the connection, every subsequent
   `read_tls` on the already-closed socket returned `Ok(0)` instantly and the
   loop busy-spun at ~100% CPU forever — a genuine live-lock, confirmed via
   `gdb`'s `thread apply all bt` (the main thread stuck in a tight
   `read_tls`/`process_new_packets` cycle, not blocked in a syscall).
   `http_url_connection::perform`'s own inline handshake loop has the
   identical gap, bounded only by its separate `HANDSHAKE_TIMEOUT` deadline
   check (so it was a wasted busy-spin there, not an outright hang). Fixed
   in both places by checking for a zero return and failing immediately with
   a clear "connection closed by peer during handshake" error, classified as
   `SSLHandshakeException` via the existing `TLS_HANDSHAKE_FAILURE_SENTINEL`
   convention. This fix is valuable independent of the cipher-restriction
   work — any future code path that can cause a real rejected-and-closed
   handshake would have hit the same live-lock.

**The up-call mechanism's blast radius had to be narrowed twice** after
regression-testing against the ~15 other Tomcat test files that call
`TesterSupport.configureClientSsl()` (all of which install the identical
`ClientSSLSocketFactory` wrapper, even though only two ever restrict
ciphers):
- An unconditional up-call whenever any real (non-placeholder)
  `SSLSocketFactory` subclass was installed exposed a **pre-existing
  classloading/vtable-install lock-ordering deadlock** (confirmed via `gdb`:
  a self-consistent-looking but unresolved contention between
  `vtable_manager`'s write lock, taken in
  `cratonvm_vm::runtime::vtable::vtable_install_adapter`, and multiple
  threads blocked acquiring it) on `TestClientCert`'s very first test — the
  baseline binary runs it cleanly. This is VM-core locking code with no
  connection to TLS; root-causing and fixing it safely was judged out of
  scope for this session (see `vm/src/runtime/vtable.rs`,
  `cratonvm_classloading::class_manager::define_class_with_options` for a
  future investigation).
- Narrowing the up-call to fire only when the installed factory's `ciphers`
  field (`TesterSupport.ClientSSLSocketFactory`'s real, private `String[]`,
  set by `setCipher()`) is non-null fixed `TestClientCert` (back to the
  known 5/18) but left `TestSSLHostConfigCompat` regressed (23/78 failing
  vs. baseline's 20/78) — its `testHost*With*Client` cases call `setCipher`
  directly with classic `TLS_DHE_RSA_*` names, the same unmappable-in-rustls
  family as `testTls12CipherNotAvailable`. Narrowing further to check
  `t27_tls::any_cipher_mappable` (already needed inside
  `setEnabledCipherSuites` to skip DHE restrictions it can't enforce) BEFORE
  deciding to up-call at all — not just before reconnecting — routes these
  DHE-only callers through the byte-identical original code path, since
  up-calling could never have helped them anyway.

**Verified:** `TestSSLHostConfigCipher` 1/12 fail (only the DHE case, no
hang — stable across multiple runs). `TestClientCert` 5/18 fail (matches
baseline). `TestCustomSslTrustManager` 2/9 fail (matches baseline; one
isolated run hit the already-documented `testCustomTrustManagerNone`
order-dependent flake, which did not recur on immediate rerun of the
identical binary). `TestCustomSsl` 1/1 fail (matches baseline).
`TestClientCertTls13`/`TestAlpnFallback` clean (match baseline).
`TestSSLHostConfigCompat` could not be fully confirmed clean: it went from
23/78 (before the mappability-gate narrowing) down to 21/78 immediately
after, with the *specific* differing tests changing between consecutive runs
of the identical binary — consistent with timing-sensitive flakiness rather
than a deterministic regression, but not conclusively distinguished from one,
because the shared Azure host was under extreme, unrelated concurrent load
for the remainder of this session (multiple other agent sessions running
Spring test sweeps and this doc's own concurrent OCSP session; `uptime` load
average peaked at 138 on a 16-core box).
**Re-verify `TestSSLHostConfigCompat` against baseline on a quiet host before
treating residual #3's TLS 1.3 fix as fully regression-clean.**

**TLS 1.2 DHE case (`testTls12CipherNotAvailable`) is NOT fixable with this
codebase's current dependencies, confirmed via direct inspection of both
libraries' source**: rustls (both crypto providers this project can use,
`ring` and `aws-lc-rs`) has never implemented classic finite-field DHE key
exchange — only ECDHE and TLS 1.3 AEAD suites (`crypto/ring/tls12.rs`
contains zero `TLS_DHE_*` entries) — a deliberate upstream project decision,
not a gap to work around. `native-tls` 0.2's public API has no cipher-suite
configuration at all (checked its full `TlsConnectorBuilder` surface: only
min/max protocol version, identity, roots, ALPN, SNI). Closing this
specific case would require adding a direct OpenSSL binding (the project
currently only pulls OpenSSL transitively through `native-tls`'s Linux
backend, not as a directly-usable dependency) — a materially larger,
separate undertaking, not a bug fix.

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

1. ~~Implement real OCSP/CRL revocation checking~~ — **DONE**
   (OCSP; see "OCSP revocation checking implementation" above). Real CRL
   fetch/parse remains a documented gap (`PREFER_CRLS`/`NO_FALLBACK` currently
   collapse to "OCSP only") — worth its own follow-up if a workload actually
   exercises CRL-preferring configurations, but no test in this suite does.
2. ~~`TestClientCert`/`TestCustomSslTrustManager`'s issuer-check gap
   (client-side `KeyManager.chooseClientAlias` never consulted)~~ —
   **IMPLEMENTED and verified via direct repro** (see "Residual #2
   implementation" above). What's left: (a) confirm end-to-end on the actual
   Tomcat suite once the classloading/vtable-install deadlock (item #7,
   shared with the cipher-restriction session) is fixed, and (b) — the
   actual current blocker — root-cause why Tomcat's connector never sets
   `need`/`want` client auth to `true` in this environment (a separate,
   Tomcat-configuration-path bug, not a `chooseClientAlias` issue; see
   "Residual #2 implementation" point 2 for exactly what to check:
   `SSLHostConfig`/`preemptiveAuthentication`/`SSLAuthenticator` →
   `SSLEngine.setNeedClientAuth`/`setWantClientAuth`/`setSSLParameters`).
3. ~~Client-side cipher-suite restriction~~ — CLOSED for TLS 1.3 (see
   "Residual #3 closeout"); the TLS 1.2 DHE case is permanently unfixable
   without adding a direct OpenSSL dependency (substantial, separate
   undertaking, not recommended unless a real caller needs DHE specifically).
4. ~~If `get_km_id`/`set_km_id` (KeyManager id, `x509_manager.rs`) is ever
   implicated in a bug report, apply the same identity-hash-side-table fix as
   `get_tm_id`/`set_tm_id` preemptively~~ — **DONE**, as part of the
   `chooseClientAlias` work above (it was implicated immediately).
5. A companion Linux native-registration gap was found (and is being fixed in
   a separate, parallel session) while iterating on this feature's test
   suite: `sun/nio/ch/UnixDispatcher.close0`/`preClose0` are missing native
   registrations, surfacing as `UnsatisfiedLinkError` during NIO
   `ServerSocket.close()` teardown (e.g. `TestOcspTimeout`'s `@AfterClass`
   responder-shutdown hook) — did not block any of this session's target
   suites (none of them hit the affected teardown path), but is a real,
   separate bug; see whichever branch/commit closes it for details.
6. **Re-verify `TestSSLHostConfigCompat` on a quiet (uncontended) host.** The
   `fix/tls-client-cipher-restriction-20260706` session could not conclusively
   distinguish "clean" from "timing-flaky under extreme shared-host load" for
   this file — see "Residual #3 closeout" for the exact numbers and what to
   check.
7. **The classloading/vtable-install lock-ordering deadlock found this
   session is a real, separate, VM-core bug**, independent of anything TLS-
   related — it happens to have been discovered via an up-call from
   `http_url_connection.rs`, but the actual bug is in
   `cratonvm_vm::runtime::vtable`/`cratonvm_classloading::class_manager`'s
   locking, likely a lock-ordering inversion between `vtable_manager` and
   `class_manager` (the interpreter's cached-dispatch fast path in
   `interpreter.rs` around line 26800 takes `vtable_manager.read()` then
   `class_manager.read()`; `vtable_install_adapter` is reached from
   `class_manager`'s own `define_class_with_options`, suggesting the reverse
   order elsewhere). Confirmed via `gdb` on `TestClientCert`'s first test
   with a real up-call reproduction case in hand (this session's original,
   unnarrowed `huc_upcall_create_socket_if_custom_factory`) — worth its own
   dedicated session with proper concurrency debugging tools, since any
   other code path that triggers class initialization re-entrantly during
   another class's `<clinit>` could hit the same deadlock.

## Post-merge finding: severe, separate, pre-existing regression affecting ALL native HttpsURLConnection connections (2026-07-07, discovered while re-verifying this merge against current dev)

After merging this session's `chooseClientAlias` fix into `dev` (bringing in ~15 unrelated commits merged by other concurrent sessions since this branch was forked, including `60e2b20d`/`48b3c2d2` "Fix class_manager/vtable_manager AB-BA lock-order deadlock" — likely fixing the classloading/vtable-install deadlock this doc and the cipher-restriction session both hit, worth re-testing), re-running the standalone repro this session used to verify the `chooseClientAlias` fix started failing with `SSLHandshakeException: handshake read: Resource temporarily unavailable (os error 11)` — deterministically (10/10 repeated attempts), and **on a plain, non-mTLS HTTPS connection with no client certificate involved at all** (confirmed via `/tmp/tlsrepro/TlsRepro3.java`, a minimal `HttpsServer` + `HttpsURLConnection` repro with no `KeyManager`/`chooseClientAlias` in play whatsoever).

This is confirmed **NOT caused by this session's fix**: an isolated build of this fix on its original base commit (`40df7946`, before any of the ~15 intervening commits) passes the same repro cleanly and repeatably; the EAGAIN failure was already present on an intermediate base (`a24035f2` plus one cherry-picked unrelated fix) built *before* this fix was merged onto it. The regression was introduced by one of the commits between `40df7946` and current `dev` HEAD — not yet root-caused (a quick look at `native-io/src/net.rs` and the JIT-signal/TLAB-refill commits in that range didn't turn up an obvious cause; the failure is immediate, not a real multi-second timeout expiry, which argues against a signal-interruption theory and for a more direct socket-configuration or read-loop logic change instead).

**This is significantly more severe than anything else in this doc**: it appears to break every native `HttpURLConnection`/`HttpsURLConnection` HTTPS request in the current `dev` tip, not just mTLS-related ones — likely affecting a large number of currently-passing test suites across the whole codebase that make any real HTTPS call through this path. Given the severity and that it's unrelated to this doc's own scope, this needs an **immediate, dedicated, high-priority follow-up session** to bisect the exact commit and fix it — recommend starting with `git bisect` between `40df7946` and current `dev` HEAD using `/tmp/tlsrepro/TlsRepro3.java` (compile with the real JDK, run with `CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`, expect `RC=200` on success) as the bisection probe — it reproduces in ~1s per attempt and has already been confirmed deterministic.
