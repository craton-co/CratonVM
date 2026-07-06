# TLS/mTLS/OCSP validation doesn't reject invalid handshakes (security-relevant)

**Status:** OPEN. **Severity:** high (fail-open on certificate/revocation
validation — connections that should be rejected are instead accepted).
**Related, but distinct:** [BUG-DF06](../internal/CRATONVM_BUGS/BUG-DF06-certpath-pkix-not-implemented.md)
(FIXED — routed `CertPathValidator PKIX` to the real Sun SPI, which stopped
these tests from aborting outright) and
[BUG-DF02](../internal/CRATONVM_BUGS/BUG-DF02-stale-zeroed-oop-receiver-dispatch-segv.md)
(OPEN — a different, memory-safety bug that DF06's own doc predicted these
tests would hit next; they do not — see below).

## Summary

DF06 fixed the `CertPathValidator PKIX` abort (`TestOcspEnabled` etc. used to
die with `runtime error: not implemented`). With that gap closed, these same
tests now run to completion — but they fail a **different** way: the TLS/mTLS
handshake or client-certificate check that the test expects to be **rejected**
is instead **accepted**. This is a fail-open validation gap, not the DF02
memory-corruption bug DF06's doc anticipated as the likely residual.

Found via a full 651-class Apache Tomcat suite rerun (`osr600verify`, real
JDK, JIT on, `CRATONVM_JIT_OSR=1`, 600s timeout, dev `8cfd53bf`+); all 9
classes below PASS on HotSpot.

## Affected classes (9) — two symptom variants of the same gap

**(a) "Handshake did not fail when expected to do so"** — a handshake that
should be rejected (bad/revoked cert, disallowed cipher, missing client cert)
completes successfully instead:
```
java.lang.AssertionError: Handshake did not fail when expected to do so.
```
- `org.apache.tomcat.util.net.ocsp.TestOcspEnabled`
- `org.apache.tomcat.util.net.ocsp.TestOcspSoftFailInternalError`
- `org.apache.tomcat.util.net.ocsp.TestOcspSoftFailTryLater`

**(b) "Expected exception: javax.net.ssl.SSLHandshakeException"** — same
shape, JUnit's `assertThrows`-style wrapper instead of a bespoke assertion:
```
java.lang.AssertionError: Expected exception: javax.net.ssl.SSLHandshakeException
```
- `org.apache.tomcat.util.net.ocsp.TestOcspSoftFail`
- `org.apache.tomcat.security.TestSecurity2017Ocsp`
- `org.apache.tomcat.util.net.TestSSLHostConfigCipher`
- `org.apache.tomcat.util.net.TestSslHandshakeFailure` (`testMissingClientCertificate`
  — a handshake with **no client certificate presented** still succeeds when
  the server config requires one)

**(c) "Checking requested client issuer against ..."** — client-certificate
issuer validation itself is wrong (not just "doesn't reject", the issuer
comparison logic produces the wrong answer):
```
java.lang.AssertionError: Checking requested client issuer against
  CN=Apache Tomcat Test CA,OU=Apache Tomcat PMC,O=The Apache Software Foundation,L=Wilmington,ST=DE,C=US
```
- `org.apache.tomcat.util.net.TestClientCert`
- `org.apache.tomcat.util.net.TestCustomSslTrustManager`

## Why this is likely one gap, not nine unrelated bugs

All 9 sit in the same code path — Tomcat's JSSE/mTLS handshake validation,
downstream of the now-functioning `CertPathValidator PKIX` (DF06). The
consistent shape ("something that should cause rejection doesn't") across
OCSP soft-fail, cipher restriction, missing-client-cert, and issuer-check
scenarios suggests the actual **enforcement** step — whatever in CratonVM's
JSSE/TrustManager or OpenSSL-bridge layer is supposed to throw/reject after a
validation check comes back negative — isn't wired up or is silently
swallowing the negative result. DF06 fixed *resolution* (the SPI now exists
and runs); this bug is in *enforcement* (the SPI's answer doesn't propagate to
an actual handshake abort).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
$env:CRATONVM_JIT_OSR = '1'   # not required to reproduce — also fails with OSR unset
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName tlsgap `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.tomcat.util.net.TestSslHandshakeFailure — testMissingClientCertificate
# is the clearest single-method repro: a handshake with no client cert should
# fail server-side validation and doesn't.
```
Baseline confirming HotSpot passes all 9: `apps/tomcat/.suite/results/overnight0629c/hotspot-jit/results.csv`.
Full run showing all 9: `apps/tomcat/.suite/results/osr600verify/real-jit/results.csv`.

## Recommendation

Investigate CratonVM's TrustManager/SSLEngine handshake-validation wiring:
find where a negative `CertPathValidator`/OCSP-checker/cipher-policy result
should raise `CertificateException`/`SSLHandshakeException` during the
handshake and confirm that exception actually propagates and aborts the
handshake, rather than being caught, logged, or ignored. `TestClientCert`'s
issuer-string mismatch suggests the client-cert-issuer comparison itself may
also have a separate, smaller logic bug worth checking independently once the
enforcement gap is fixed (the two symptom groups may or may not share a single
root cause — verify rather than assume once the fix lands).
