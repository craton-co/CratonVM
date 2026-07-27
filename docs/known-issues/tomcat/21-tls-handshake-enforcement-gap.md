# TLS handshake enforcement/validation gap — 8 classes

**Status:** OPEN. Confirmed CratonVM-only regression — all 8 classes PASS
cleanly on real JDK 25 (HotSpot) in the same fixture; CratonVM either
enforces a rejection condition too loosely (accepts a handshake HotSpot
correctly refuses) or throws the wrong exception type for one.

Reproduced consistently across two independent runs on the local Windows
harness (`../../../apps/tomcat`, `../../../apps/tomcat-suite-runner`), `dev` tips ~60a710ad8
and ~12c79a0ee (250+ commits apart) — same symptoms both times, so this is
not a transient/contention artifact.

## Two symptom directions, likely one shared root cause in the TLS handshake path

**A. Handshakes that should be REJECTED are ACCEPTED, or throw the wrong exception:**

- `org.apache.tomcat.util.net.TestSsl` — `testSni[JSSE]`:
  `AssertionError: expected:<400> but was:<200>` (an SNI-mismatch request
  that should be rejected with 400 gets served normally).
- `org.apache.tomcat.util.net.TestSSLHostConfigCompat` —
  `testHostECwithRSAClient[JSSE-KEYSTORE]` (and 4 more parameterized cases):
  `AssertionError: Expected exception: javax.net.ssl.SSLHandshakeException`
  (none thrown — an EC-host/RSA-client mismatch that should fail the
  handshake succeeds instead).
- `org.apache.tomcat.util.net.TestSSLHostConfigCipher` —
  `testTls13CipherNotAvailable[JSSE]` (+1 more): same "expected exception,
  none thrown" pattern for a disallowed TLS 1.3 cipher.
- `org.apache.tomcat.util.net.TestSSLHostConfigProtocol` —
  `testTlsVersionMismatchServerTls12ClientTls13[JSSE]` (+1 more): same
  pattern for a TLS version mismatch that should be refused.
- `org.apache.tomcat.util.net.TestSslHandshakeFailure` —
  `testMissingClientCertificate`: `Exception: Unexpected exception,
  expected<SSLHandshakeException> but was<IOException>` — the handshake
  DOES fail (good), but with the wrong exception type.

**B. Handshakes that SHOULD succeed (legitimate client certificate) are being
rejected:**

- `org.apache.tomcat.util.net.TestClientCert` —
  `testClientCertGetWithPreemptive[JSSE]` (+4 more):
  `SSLHandshakeException: connection closed immediately after the TLS
  handshake with no response`.
- `org.apache.tomcat.util.net.TestCustomSslTrustManager` —
  `testCustomTrustManagerCA[JSSE]` (+1 more): identical "connection closed
  immediately" symptom.
- `org.apache.catalina.valves.rewrite.TestResolverSSL` — `testSslEnv[JSSE]`:
  identical "connection closed immediately" symptom.

## Why this looks like one bug, not eight

Every failure is a validation/rejection-boundary condition specific to TLS
handshake setup: SNI host matching, cert/key-algorithm compatibility,
cipher/protocol allow-lists, and client-certificate presentation/
verification. Both directions (too permissive AND too strict) point at the
same area — the real-socket JSSE emulation layer's `SSLEngine`/
`SSLContext`/`X509TrustManager` wiring not faithfully replicating the exact
decision points HotSpot's real JSSE implementation uses, rather than eight
unrelated defects. Not root-caused down to a specific file/line in this
session — that's the next step for whoever picks this up.

## Reproduction

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <path-to-a-results-csv-marking-these-8-non-PASS> -TimeoutSec 300 -Parallel 4 -RunName tls-repro -Exe <cratonvm.exe>
```
Real JDK 25 boot, real sockets (`CRATONVM_REAL_NET_SOCKETS=1`, set
automatically by `run-tomcat-suite.ps1` for `-Vm craton`). Compare against
`-Vm hotspot` on the same class list — all 8 pass.

## Note: two DIFFERENT, already-fixed `TestSsl` bugs are NOT this issue

A concurrent session already fixed two unrelated `TestSsl` bugs this week
(missing `SSLSocket.addHandshakeCompletedListener` native registration;
`rustls_stream_read` not tolerating a peer closing without `close_notify`)
— see `../../internal/fixed-suite-bugs/tomcat/20-fixture-completion-regressions-closure-FIXED.md`.
Neither explains `testSni`'s `400`-vs-`200` symptom above, which is a
separate, still-open issue in the same class.
