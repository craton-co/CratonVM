# TLS handshake enforcement: 4 classes where a handshake CratonVM's TLS stack allows/rejects diverges from JSSE's — OPEN, not yet root-caused

## Status
**OPEN.** Candidate real behavioral differences between CratonVM's TLS
backend and JSSE, found via a c1/c2 JIT-tier suite run — neither arm is a TLS
variable, so these are not JIT-tier-dependent, but they have **not been run
against a HotSpot control on this fixture** and per this suite's own
documented rule (`run-tomcat-suite.md` §4 / `nonpassed-class-census.md`)
should not be called CratonVM regressions until that control is run. Filed
now because these are new, not because they're confirmed.

## Measured
2026-09-12, `dev@c0bebbde5`, local Windows box, real JDK 25, `-Parallel 2`,
two JIT-tier arms (`CRATONVM_C2_SUPERSEDE=0`, `CRATONVM_JIT_FORCE_C2=1`),
651-class complete suite each. All 4 classes below fail identically in both
arms (same test names, same failure counts, same exception shape) —
consistent with a TLS-stack behavior, not a JIT artifact.

## Group 1 — an expected `SSLHandshakeException` never arrives (3 classes, 8 sub-tests)

| class | failing test(s) | asserts the handshake should be REFUSED for |
|---|---|---|
| `TestSSLHostConfigCipher` | `testTls13CipherNotAvailable[JSSE]`, `testTls12CipherNotAvailable[JSSE]` | a cipher suite not enabled on that host config |
| `TestSSLHostConfigCompat` | `testHostECwithRSAClient[JSSE-KEYSTORE\|PEM]`, `testHostRSAwithECClient[JSSE-KEYSTORE\|PEM]` | an EC client cert against an RSA-only host (and vice versa) |
| `TestSSLHostConfigProtocol` | `testTlsVersionMismatchServerTls12ClientTls13[JSSE]`, `...ServerTls13ClientTls12[JSSE]` | a client/server TLS version that don't overlap |

All 8 fail with the identical shape:
```
java.lang.AssertionError: Expected exception: javax.net.ssl.SSLHandshakeException
	at org.junit.internal.runners.statements.ExpectException.evaluate(ExpectException.java:34)
```
i.e. the test expects the handshake to be rejected and it succeeds instead.
This is the opposite direction from the already-documented, accepted rustls
limitation in `ssl-renegotiation-emulation-limits.md` (which is about a
handshake CratonVM cannot avoid completing because rustls has no TLS 1.2
renegotiation) — that page is about a rejection CratonVM can't perform after
the fact; this is about three different kinds of pre-handshake negotiation
mismatch (cipher availability, key-type/cert compatibility, protocol version
overlap) apparently not being enforced at all. Not yet traced into
`native-builtins-crypto`/the rustls config-building path to find which
config knob (`ClientHello` cipher/version offer filtering, or the host's
`ServerConfig` acceptance) is too permissive.

**Not the classpath gap — separated 2026-09-12.** This page used to leave open
whether the 8 sub-tests were a side effect of the stale BouncyCastle/UnboundID
classpath (`docs/internal/tomcat/cp-txt-stale-gradle-module-cache-paths-FIXED-20260912.md`).
With that classpath restored, the three classes were rerun one process each,
same JVM arguments as the suite: HotSpot **OK (12) / OK (78) / OK (12)** for
`Cipher`/`Compat`/`Protocol`, CratonVM the same **2 / 4 / 2** sub-tests
failing with the same `Expected exception: javax.net.ssl.SSLHandshakeException`.
Group 1 is therefore also past its HotSpot control: HotSpot enforces all 8
refusals on this fixture and CratonVM does not.

## Group 2 — a handshake fails when it's expected to succeed (1 class, 5 sub-tests)

`org.apache.tomcat.util.net.ocsp.TestOcspEnabled` — 5 of 116 sub-tests, all
the same shape:
```
java.lang.AssertionError: Handshake failed when not expected to do so.
	at org.apache.tomcat.util.net.ocsp.TestOcspEnabled.test(TestOcspEnabled.java:117)
```
All 5 failing parameterizations share `JSSE with OpenSSL trust false ...
serverOk true ... verifyServer true` — i.e. cases where the server's OCSP
status should validate cleanly and client trust succeeds, but the handshake
is refused anyway. This is the opposite direction from Group 1 (over-strict
here, not over-permissive) and from the already-fixed/closed
`ocsp-trust-check-parked-a-mutator-the-collector-still-counted-FIXED-20260822.md`
(that was a hang inside OCSP fetch, not a wrongly-refused handshake) — kept
in this page rather than a separate one only because it's in the same
`TesterKeystoreGenerator`/`util.net` family and was found in the same run;
split it out if it turns out to share no mechanism with Group 1.

## What this page is NOT saying
Not confirmed as CratonVM defects. Per this suite's own rule, the next step
for both groups is a same-fixture HotSpot control pass over these 5 class
names before treating any of them as more than a candidate. Not run in this
session.

## Repro
```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Category all -Start <index> -Count 1 -RunName repro -TimeoutSec 300
# or -Vm hotspot for the control this page is missing
```
