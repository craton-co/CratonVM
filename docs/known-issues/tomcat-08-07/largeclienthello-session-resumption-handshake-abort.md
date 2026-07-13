# TestLargeClientHello — session-resumption handshake aborted by server

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS.
**Found:** 2026-07-13, newly VISIBLE (not newly caused) — this class
previously died earlier with the `NoSuchMethodError: java/lang/String.size()I`
shutdown failure (now fixed — see
`docs/internal/tomcat-08-07/largeclienthello-string-size-nosuchmethod-FIXED.md`).

## Summary

`org.apache.tomcat.util.net.TestLargeClientHello` now runs both tests;
`testLargeClientHello` (plain oversized ClientHello) PASSES, but
`testLargeClientHelloWithSessionResumption` fails deterministically
(2/2 reruns, both under load and quiet):

```
1) testLargeClientHelloWithSessionResumption(org.apache.tomcat.util.net.TestLargeClientHello)
javax.net.ssl.SSLHandshakeException: connection closed by peer during handshake
	at org.apache.catalina.startup.TomcatBaseTest.methodUrl(TomcatBaseTest.java:709)
	at org.apache.tomcat.util.net.TestLargeClientHello.testLargeClientHelloWithSessionResumption(TestLargeClientHello.java:77)
```

(One run showed the WinSock flavor `handshake read: ... (os error 10053)`,
the other "connection closed by peer during handshake" — both are the
server side dropping the second, resumption-bearing handshake.)

The test performs a first TLS connection (works — the plain test and the
first leg pass), then reconnects with a session-resumption attempt plus a
padded ClientHello larger than one TLS record. The server-side TLS stack
(rustls-backed JSSE shim) aborts that second handshake. Likely suspects:
session ticket/ID resumption not supported or mis-negotiated by the shim
(cf. `reference_rustls_no_dhe_support` for a prior permanent rustls
capability gap), or multi-record ClientHello parsing failing specifically
on the resumption path.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName lch `
  -Start 599 -Count 1 -TimeoutSec 600 -Parallel 1
# org.apache.tomcat.util.net.TestLargeClientHello (index 599 as of 2026-07-13)
```

## Recommendation

Capture the server-side rustls handshake error (enable the TLS shim's
debug logging) for the second connection; check whether the shim's server
config enables session resumption (tickets/cache) at all, and whether the
fragmented ClientHello reassembly path differs when a session ticket
extension is present.
