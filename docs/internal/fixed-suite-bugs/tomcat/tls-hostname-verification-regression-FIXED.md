# TLS hostname verification regression — breaks all 7 classes doc 21 fixed

**Status: RESOLVED (2026-08-01).** All 7 classes rechecked on a fixed binary;
not one of their logs still contains the hostname-verifier message.

This doc's diagnosis in "Why this is a genuinely new, single defect" was
correct on every point: one shared mechanism, client-side, and hostname
verification "happening on a path where it wasn't the deciding factor before".

The root cause, the fix and the A/B are written up once, in
`docs/internal/fixed-suite-bugs/springboot/simpleclienthttprequestfactory-app-hostnameverifier-rejects-localhost-FIXED.md`
— the same defect was filed independently from the Spring Boot side. In short:
the real JDK's default `HostnameVerifier` is a hardcoded `return false`, it was
NOT recognised as a default stand-in, and it was being consulted as a mandatory
gate rather than as the fallback JSSE actually makes it.

Results (one process per class, fixed binary):

```
  PASS         26,1s  org.apache.catalina.valves.rewrite.TestResolverSSL
  PASS         28,7s  org.apache.tomcat.util.net.TestCustomSslTrustManager
  PASS           18s  org.apache.tomcat.util.net.TestSslHandshakeFailure
  PASS         51,1s  org.apache.tomcat.util.net.TestSSLHostConfigCompat
  PASS         18,7s  org.apache.tomcat.util.net.TestSSLHostConfigProtocol
  PASS         21,7s  org.apache.tomcat.util.net.TestSSLHostConfigCipher
  FAIL        445,3s  org.apache.tomcat.util.net.TestSsl   (7 failures -> 1)
```

Two questions this doc raised, both now answered:

- `TestSSLHostConfigProtocol` — "worth confirming it's not silently exercising
  a code path that skips hostname verification". It is not: it passes, and its
  log carries no verifier message.
- `TestSsl`'s remaining single failure,
  `testClientInitiatedRenegotiation[JSSE]`, is **not** part of this defect — it
  fails identically on a pre-fix baseline binary. Split out to
  `docs/known-issues/tomcat/testssl-client-initiated-renegotiation-20260801.md`
  so it survives this doc's retirement.

The `git log` bisect proposed below as the next step was not needed: the
culprit commit (`f6028ba50`) was already named by the Spring Boot filing.

---

*Original report follows, unedited.*

**Status:** OPEN, new regression. Not the bug
[21-tls-handshake-enforcement-gap-FIXED.md](21-tls-handshake-enforcement-gap-FIXED.md)
fixed — a different, later-introduced defect that happens to break the same
classes doc 21 had just gotten green.

## How this was found

A 2026-07-31 full-suite rerun (`dev` merged fresh, 106 commits past doc 21's
2026-07-27 verification) showed 7 of the 8 classes doc 21 marked FIXED
failing again. Before writing this off as "same bug recurred" or a test
artifact, reran 4 of them standalone with the correct CWD and the fresh
binary — all 4 fail with the **identical** new symptom, which is not the
symptom doc 21 fixed:

```
javax.net.ssl.SSLPeerUnverifiedException: Certificate for <localhost> does not match the installed HostnameVerifier
```

- `org.apache.catalina.valves.rewrite.TestResolverSSL.testSslEnv[JSSE]` —
  doc 21 reported "OK 3/3"; now 1 failure, this exception.
- `org.apache.tomcat.util.net.TestCustomSslTrustManager.testCustomTrustManagerNone[JSSE]` —
  doc 21 reported "OK 9/9"; now fails, same exception.
- `org.apache.tomcat.util.net.TestSSLHostConfigCompat.testHostECandRSAwithRSAClient[JSSE-KEYSTORE]` —
  doc 21 reported "OK 78/78"; now fails, same exception.
- `org.apache.tomcat.util.net.TestSslHandshakeFailure.testMissingClientCertificate` —
  doc 21 reported "OK 1/1"; now fails with
  `Exception: Unexpected exception, expected<SSLHandshakeException> but
  was<SSLPeerUnverifiedException>` — same root symptom, surfaced through a
  different assertion because this test expects a *different* exception
  type in the first place.

Not individually rechecked here: `TestSSLHostConfigCipher`,
`TestSSLHostConfigProtocol` (in the 4-shard rerun this one actually PASSED —
worth confirming it's not silently exercising a code path that skips
hostname verification), `TestSsl`. All were in the same "now failing again"
set from the suite run.

## Why this is a genuinely new, single defect and not eight re-breaks

1. The exception type and message are byte-identical across every class
   checked, including one (`TestSslHandshakeFailure`) that reaches it via a
   completely different test method and assertion shape than the others —
   that consistency across unrelated call sites is the signature of one
   shared mechanism, not independent regressions in each class.
2. The symptom itself — a hostname check rejecting `localhost` against
   `localhost` — is a specific, well-known failure shape: either the
   client-side `HostnameVerifier`/certificate-hostname matching logic
   changed to be stricter than it was on 2026-07-27, or the server-side
   certificate CratonVM's TLS stack presents for these tests stopped
   carrying a SAN/CN that matches `localhost` (e.g. a keystore/certificate
   selection change, or a regression in whatever code doc 21's own fixes
   (`SSLEngine`/`SSLContext` wiring work, per its "nine distinct defects"
   writeup) touched).
3. `TestSslHandshakeFailure.testMissingClientCertificate` is the most
   informative case: it deliberately triggers a failure and asserts on the
   exception *type*. Getting `SSLPeerUnverifiedException` instead of
   `SSLHandshakeException` there means the NEW failure point is hostname
   verification happening on a path where it wasn't the deciding factor
   before — consistent with a change that made hostname verification apply,
   or apply differently, more broadly than it used to.

## Not investigated here

The actual code change responsible. Doc 21 lists nine distinct defects it
fixed in the TLS handshake path — the regression likely lives in the same
area (SSLEngine/SSLContext/certificate wiring) but which of the ~106
intervening commits touched it was not bisected. `git log --oneline
<doc21-verification-commit>..HEAD -- '**/*tls*' '**/*ssl*'` (or equivalent
for whatever native module backs the JSSE emulation) is the natural next
step, followed by an A/B on a single binary the way doc 21's own
methodology insists on.

## Reproduction

```powershell
cd apps\tomcat
$env:CRATONVM_REAL_NET_SOCKETS='1'; $env:CRATONVM_REAL_AQS='1'
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG='1'; $env:CRATONVM_ROOTSNAP_CACHE='1'
<cratonvm.exe> --Xmx 2g -c "$(Get-Content .suite\cp.txt)" `
  org.junit.runner.JUnitCore org.apache.catalina.valves.rewrite.TestResolverSSL
```
Fails in ~15s, deterministic, no timing/load dependency observed across the
checks done here.
