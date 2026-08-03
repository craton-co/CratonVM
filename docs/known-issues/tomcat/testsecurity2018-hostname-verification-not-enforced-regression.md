# `TestSecurity2018.testCVE_2018_8034` — hostname verification no longer rejects a `127.0.0.1`-vs-`localhost` certificate mismatch

| | |
|---|---|
| **Status** | OPEN — security-relevant regression |
| **Severity** | high — a hostname-verification bypass, even in a narrow scenario, is a security defect |
| **HotSpot** | PASS (`OK (1)`, fresh-verified 2026-08-03) |
| **CratonVM** | FAIL, reproduces 2/2 |
| **Discovered** | 2026-08-03, rerunning the 08-02 4-shard FAIL/HANG set (1500s timeout) after merging `dev` (`9b3b93b97`) |

## Symptom

```
1) testCVE_2018_8034(org.apache.tomcat.security.TestSecurity2018)
java.lang.Exception: Unexpected exception, expected<jakarta.websocket.DeploymentException> but was<java.lang.AssertionError>
	at org.junit.internal.runners.statements.ExpectException.evaluate(ExpectException.java:30)
Caused by: java.lang.AssertionError: Hostname verification should have failed
for 127.0.0.1 with a certificate issued for localhost only.
```

`TestSecurity2018` regression-tests CVE-2018-8034 (a historical Tomcat
WebSocket-client TLS hostname-verification bypass). The test connects to
`127.0.0.1` using a server certificate issued only for `localhost` and
expects the connection to be refused (`DeploymentException` wrapping a
hostname-verification failure). CratonVM's client now accepts the connection
instead — hostname verification is not rejecting the mismatch it is
specifically designed to catch.

## This is a genuine regression, not a stale classpath-gap symptom

This exact class previously PASSED cleanly on CratonVM (`OK (1)`, both JIT and
`--nojit`) once the BouncyCastle classpath gap was fixed — see the
verification table in
[`bouncycastle-easymock-classpath-fixture-gap-FIXED.md`](../../internal/fixed-suite-bugs/tomcat/bouncycastle-easymock-classpath-fixture-gap-FIXED.md),
measured on binary `cratonvm-bccp-20260803.exe` at `dev` commit `36f1157ad`.
The regression appeared somewhere in the ~170 commits merged into `dev`
between `36f1157ad` and the current tip (`9b3b93b97`) — not yet bisected.
`fec4d208e fix(tls): a HostnameVerifier is JSSE's FALLBACK, not an extra
gate` (the main hostname-verification fix from earlier that day) was already
included in the `36f1157ad` baseline where this test passed, so that specific
commit is **not** the cause; the regression is later.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1; $env:CRATONVM_ROOTSNAP_CACHE=1
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.tomcat.security.TestSecurity2018
```

HotSpot control: `OK (1 test)` in 2.4s.

## Suggested next step

Bisect `36f1157ad..9b3b93b97` on files touching `HostnameVerifier`,
`SSLEngine`, or the WebSocket client's TLS connect path (candidates already
ruled out: `8cf06d764` IPv4-mapped-address folding, `fedb11592` SSLEngine
wrap/FINISHED sequencing, `382b43085` Conscrypt JNI_OnLoad skip — none of
these touch hostname-verification logic on inspection, but none have been
positively excluded by bisection either). Given the security relevance,
treat as high priority relative to the other residual FAILs from this rerun.
