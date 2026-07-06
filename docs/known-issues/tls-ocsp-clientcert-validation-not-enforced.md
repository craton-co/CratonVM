# TLS/mTLS/OCSP validation doesn't reject invalid handshakes (security-relevant)

**Status:** PARTIALLY FIXED, follow-up session (branch
`fix/tls-residuals-followup-20260706`, based on the original
`fix/tls-ocsp-clientcert-validation-not-enforced-20260706` work). The
thread-local cross-contamination this doc's residuals #1/#4 blamed was a
plausible but ultimately WRONG diagnosis — the real root cause was two
separate identity/id round-trip bugs (below), now fixed and verified:
`TestOcspEnabled` improved from 15/116 to 5/116 failures, and the
order-dependent `TestCustomSslTrustManager` flakiness has not recurred across
repeated isolated reruns. The remaining 5/116 (plus `TestSecurity2017Ocsp`,
which now correctly *fails*) trace to a **newly discovered, distinct, and
more serious gap: OCSP/CRL revocation checking is not implemented at all** —
see "New finding" below. Residuals #2 (`chooseClientAlias`) and #3
(client-side cipher restriction) are untouched, confirmed still open.

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

## New finding: OCSP/CRL revocation checking is not implemented (open, more severe than the residuals it replaces)

`x509_manager::validate_chain` performs full structural PKIX validation
(chain continuity, expiry, `BasicConstraints`, name constraints, signature
verification, trust-anchor matching) but **never checks revocation status at
all** — `TrustManagerState.enable_crl` is a field that gets set (always to
`false`) and is never read anywhere in the crate. There is no OCSP responder
query, no CRL check, nothing: a structurally valid chain from a trusted CA is
accepted regardless of whether the leaf certificate has been revoked.

This was invisible before this session's fixes because `TestOcspEnabled`
resolved trust anchors incorrectly for essentially every configuration (see
above), so revoked-cert test cases "passed" by accident (rejected for the
wrong reason: unknown issuer, not revocation). With trust-anchor resolution
now correct, the previously-clean `TestSecurity2017Ocsp` (5/5 pass) now
correctly **fails** its one meaningful assertion,
`testCVE_2017_15698` (a revoked client cert must be rejected — it is now
accepted), and `TestOcspEnabled` still shows 5/116 failures, all
`serverOk=false, verifyServer=true` (a revoked *server* cert, with client-side
revocation checking enabled, is wrongly accepted) — this is a genuine
**fail-open** result, more severe than any residual this doc previously
tracked. `TestOcspSoftFail`/`TestOcspSoftFailInternalError`/
`TestOcspSoftFailTryLater` show a mix of newly-passing and newly-failing
cases consistent with the same root cause (soft-fail semantics only make
sense once revocation checking exists to soft-fail *from*).

Implementing real revocation checking (OCSP responder HTTP round-trip and/or
CRL fetch-and-parse, wired through `PKIXRevocationChecker`'s options —
`NO_FALLBACK`, soft-fail, etc.) is a substantial new feature, not a bug fix,
and needs its own dedicated session — this doc merely upgrades the
diagnosis from "unconfirmed, might be fail-closed-by-accident" (the prior
session's caveat) to "confirmed unimplemented."

### Verified test outcomes (JSSE variant; OpenSSL/OpenSSL-FFM variants still
skip — native `ssl.dll`/tomcat-native not installed, environmental, unrelated)

| Class | Before this session | After |
|---|---|---|
| `TestSslHandshakeFailure` | OK (1/1) | OK (1/1) — confirmed no regression (one batch run showed a flake, isolated rerun passed cleanly, matching this suite's known shared-host batch-contention behavior) |
| `TestSecurity2017Ocsp` | OK (5/5) (flagged as unconfirmed) | **1/5 fail** — `testCVE_2017_15698` now correctly exposes the revocation-checking gap above (not a regression — the prior "pass" was fail-closed-by-accident) |
| `TestOcspEnabled` | 15/116 fail | **5/116 fail**, all `serverOk=false,verifyServer=true` (revocation-checking gap, not the id-resolution bugs fixed this session) |
| `TestOcspSoftFail` | 2/15 fail | 1/15 fail |
| `TestOcspSoftFailInternalError` | 2/20 fail | 2/20 fail |
| `TestOcspSoftFailTryLater` | 2/20 fail | 4/20 fail (also revocation-checking-gap-shaped; not re-investigated in detail) |
| `TestClientCert` | 13/18 pass, 5/18 fail | 13/18 pass, 5/18 fail — unchanged, same known `chooseClientAlias` gap (residual #2, still open) |
| `TestSSLHostConfigCipher` | 2/12 fail | not rerun this session — no code touched here, no reason to expect a change (residual #3, still open) |
| `TestCustomSslTrustManager` | 2–3/9 fail, order-dependent | **2/9 fail, consistently** (`testCustomTrustManagerCA`/`All`, the known `chooseClientAlias` gap) — `testCustomTrustManagerNone`'s order-dependent flake did not recur |

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

1. **Implement real OCSP/CRL revocation checking** in
   `x509_manager::validate_chain` — the highest-value remaining fix, now that
   trust-anchor resolution itself is correct. `TrustManagerState.enable_crl`
   already exists as a hook point but is unused; needs an actual OCSP
   responder HTTP round-trip (or CRL fetch) wired through
   `PKIXRevocationChecker`'s configured options (`NO_FALLBACK`, soft-fail).
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
