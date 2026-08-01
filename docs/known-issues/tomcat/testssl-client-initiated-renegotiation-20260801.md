# `TestSsl.testClientInitiatedRenegotiation[JSSE]` — bare `AssertionError`

**Status: OPEN — split out 2026-08-01**

## Why this doc exists

`docs/known-issues/tomcat/tls-hostname-verification-regression-OPEN.md` listed
`org.apache.tomcat.util.net.TestSsl` among the 7 classes broken by the
hostname-verification defect. That defect is fixed (see
`docs/internal/fixed-suite-bugs/springboot/simpleclienthttprequestfactory-app-hostnameverifier-rejects-localhost-FIXED.md`)
and `TestSsl` went from **7 failures to 1**. This is the 1 that remains, split
out so retiring the hostname doc does not silently drop it.

It is **not** a residual of that fix: it fails identically on a pre-fix
binary, and neither arm's log contains the hostname-verifier message.

## Symptom

```
1) testClientInitiatedRenegotiation[JSSE](org.apache.tomcat.util.net.TestSsl)
java.lang.AssertionError
	at org.junit.Assert.fail(Assert.java:87)
	at org.junit.Assert.assertTrue(Assert.java:42)
	at org.junit.Assert.assertTrue(Assert.java:53)
```

A bare `assertTrue` with no message, so the stack alone does not say which
assertion. The test drives a **client-initiated TLS renegotiation** on an
established connection and asserts the exchange still works afterwards.

Note the runtime: the class takes ~410–445s in both arms, versus 18–51s for
the other six TLS classes. Worth checking whether this test is timing out
internally rather than genuinely asserting false.

## A/B (2026-08-01, same host, one process per class)

| binary | result |
|---|---|
| pre-fix (`dev`-equivalent) | `Tests run: 21, Failures: 7` — 6 hostname + this one |
| post-fix, 3 of 4 runs | `Tests run: 21, Failures: 1` — only this one |
| post-fix, 1 of 4 runs | `Failures: 2` — this one plus a `testPost` load flake |

This test failed in **every** post-fix run (4/4), so unlike `testPost` it is
not load-dependent.

## Sibling flake, deliberately not filed as a defect

`testPost[JSSE]` failed once in four post-fix runs, on the run that overlapped
a concurrent cargo build on this shared host. Its thread-level errors are
`java.io.IOException` (`os error 10053`, connection aborted) and
`TLS connect: connection closed by peer during handshake` — accept-path
saturation in a test that fans out many concurrent TLS POSTs, not a TLS
correctness problem. Class runtime swung 270–445s across the same runs. Noted
here so a future run that sees `Failures: 2` does not read it as a regression.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$env:CRATONVM_REAL='net-sockets,aqs'; $env:CRATONVM_THREADS='-default-watchdog'; $env:CRATONVM_JIT='rootsnap-cache'
<cratonvm.exe> --java-home "<jdk>" --Xmx 2g -Dtomcat.test.basedir=output\build -Dtomcat.test.relaxTiming=true `
  -cp "$(Get-Content .suite\cp.txt)" org.junit.runner.JUnitCore org.apache.tomcat.util.net.TestSsl
```

## Suggested next step

Read the assertion by line number in
`test/org/apache/tomcat/util/net/TestSsl.java`'s
`testClientInitiatedRenegotiation` to learn which of its checks fails, then
compare against a stock-HotSpot control run of the same class — Tomcat
disables client-initiated renegotiation by default on some connectors, so
confirm the expected outcome before assuming CratonVM is wrong.
