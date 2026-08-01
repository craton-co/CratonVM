# `TestSSLHostConfigCompat.testHostEC[JSSE-KEYSTORE]` — intermittent `Read timed out` (~40%)

**Status: OPEN — newly EXPOSED 2026-08-01**

## Why this doc exists

Fixing the hostname-verification defect
(`docs/internal/fixed-suite-bugs/springboot/simpleclienthttprequestfactory-app-hostnameverifier-rejects-localhost-FIXED.md`)
took `TestSSLHostConfigCompat` from **22 failures to 0–1**. This is the 0–1.

It is **newly exposed, not newly caused**. Pre-fix, all 22 failures carried the
hostname-verifier message and **zero** carried a read timeout — the class never
got past endpoint identification, so this flake could not be observed. The fix
touches only endpoint identification against already-received DER bytes; it
performs no I/O and cannot produce a socket read timeout.

## Symptom

```
1) testHostEC[JSSE-KEYSTORE](org.apache.tomcat.util.net.TestSSLHostConfigCompat)
java.net.SocketTimeoutException: Read timed out
	at org.apache.catalina.startup.TomcatBaseTest.methodUrl(TomcatBaseTest.java:709)
	...
	at org.apache.tomcat.util.net.TestSSLHostConfigCompat.doTest(TestSSLHostConfigCompat.java:304)
	at org.apache.tomcat.util.net.TestSSLHostConfigCompat.testHostEC(TestSSLHostConfigCompat.java:80)
```

Always the same single test, `testHostEC[JSSE-KEYSTORE]`. `Tests run: 78,
Failures: 1` when it fires.

## Measurements (2026-08-01, same host)

| arm | result |
|---|---|
| pre-fix CratonVM (`dev`-equivalent, TLS files identical) | `Failures: 22` — **all 22** the hostname-verifier message, **0** read timeouts |
| post-fix CratonVM, 5 runs | 3× `OK 78/78`, 2× `Failures: 1` (this test) |
| **stock HotSpot control, JDK 25** | **`OK (78 tests)`** |

So ~40% flaky on CratonVM, clean on HotSpot. Not load-dependent in the usual
way: one failing run took 326s (host busy), but another failed in **25.5s**
with the host idle, and passing runs took 22–51s.

## A lead worth checking first

`testHostEC` is the EC-keystore variant. `test/org/apache/tomcat/util/net/localhost-ec.jks`
carries **`CN=localhost` with no SubjectAltName at all**, unlike
`localhost-rsa.jks` (`SAN dNSName=localhost, IPAddress=127.0.0.1`). That makes
it the one certificate in this suite that takes the legacy commonName-fallback
branch of `x509_manager::verify_hostname`.

That branch is exercised and returns success — the failure is a *timeout*, not
an `SSLPeerUnverifiedException`, so identification passed. But the EC/no-SAN
combination being the single flaky variant is too specific to dismiss; check
whether the EC handshake path (not the name check) is what's intermittently
stalling.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$env:CRATONVM_REAL='net-sockets,aqs'; $env:CRATONVM_THREADS='-default-watchdog'; $env:CRATONVM_JIT='rootsnap-cache'
<cratonvm.exe> --java-home "<jdk>" --Xmx 2g -Dtomcat.test.basedir=output\build -Dtomcat.test.relaxTiming=true `
  -cp "$(Get-Content .suite\cp.txt)" org.junit.runner.JUnitCore org.apache.tomcat.util.net.TestSSLHostConfigCompat
```

Expect to need 2–3 runs to see it. Compare against the same command with
stock `java.exe` (drop `--java-home`, use `-Xmx2g`), which passes 78/78.
